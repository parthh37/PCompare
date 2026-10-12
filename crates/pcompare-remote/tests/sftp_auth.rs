//! SSH keyboard-interactive sign-in against an in-process server (russh +
//! russh-sftp), since a stock `sshd` only offers it through PAM, which a
//! user-level test server can't use. The server asks for a password, then a
//! verification code, before serving a two-file SFTP root.

use pcompare_remote::profile::{MemorySecrets, ProfileInput};
use pcompare_remote::prompt::{AuthPrompt, Prompter};
use pcompare_remote::provider::ProviderKind;
use pcompare_remote::RemoteManager;
use russh::server::{Auth, Msg, Response, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use russh_sftp::protocol::{File, FileAttributes, Handle, Name, Status, StatusCode, Version};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A throwaway host key for this test server only.
const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAnt5/1oZ3PTbl8dwOuOzYOXerQ/MZxOrJFMDNUmdK7egAAAJgAH0LsAB9C
7AAAAAtzc2gtZWQyNTUxOQAAACAnt5/1oZ3PTbl8dwOuOzYOXerQ/MZxOrJFMDNUmdK7eg
AAAEBolAqsgCBOB+p0aidPY2R4gIvGcsl227ocuHhfLcXdDCe3n/Whnc9NuXx3A647Ng5d
6tD8xnE6skUwM1SZ0rt6AAAAEnBjb21wYXJlLXRlc3QtaG9zdAECAw==
-----END OPENSSH PRIVATE KEY-----
";

const PASSWORD: &str = "hunter2";
const CODE: &str = "123456";

#[derive(Clone)]
struct Server;

impl russh::server::Server for Server {
    type Handler = Ssh;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Ssh {
        Ssh {
            step: 0,
            channels: HashMap::new(),
        }
    }
}

struct Ssh {
    /// Keyboard-interactive progress: 0 = nothing asked, 1 = password asked, 2 = code asked.
    step: u8,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

fn prompt(text: &'static str, echo: bool) -> Auth {
    Auth::Partial {
        name: Cow::Borrowed(""),
        instructions: Cow::Borrowed(if echo {
            "Enter the code from your authenticator app."
        } else {
            ""
        }),
        prompts: Cow::Owned(vec![(Cow::Borrowed(text), echo)]),
    }
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    // Like a PAM-only server: plain passwords are refused in favour of
    // keyboard-interactive.
    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..])),
            partial_success: false,
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let answer = response
            .and_then(|mut r| r.next())
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        Ok(match (self.step, answer.as_deref()) {
            (0, _) => {
                self.step = 1;
                prompt("Password: ", false)
            }
            (1, Some(PASSWORD)) => {
                self.step = 2;
                prompt("Verification code: ", true)
            }
            // A wrong password is asked for again, as PAM does.
            (1, Some(_)) => prompt("Password: ", false),
            (2, Some(CODE)) => Auth::Accept,
            _ => {
                self.step = 0;
                Auth::reject()
            }
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        match self.channels.remove(&id).filter(|_| name == "sftp") {
            Some(channel) => {
                session.channel_success(id)?;
                russh_sftp::server::run(channel.into_stream(), Sftp::default()).await;
            }
            None => session.channel_failure(id)?,
        }
        Ok(())
    }
}

/// Serves a root folder holding `a.txt` and `b.txt`.
#[derive(Default)]
struct Sftp {
    listed: bool,
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _version: u32, _ext: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, _path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy("/")],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.listed = false;
        Ok(Handle { id, handle: path })
    }

    async fn readdir(&mut self, id: u32, _handle: String) -> Result<Name, Self::Error> {
        if std::mem::replace(&mut self.listed, true) {
            return Err(StatusCode::Eof);
        }
        let file = |name: &str| {
            let mut attrs = FileAttributes {
                size: Some(3),
                ..Default::default()
            };
            attrs.set_regular(true);
            File::new(name, attrs)
        };
        Ok(Name {
            id,
            files: vec![file("a.txt"), file("b.txt")],
        })
    }

    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        })
    }
}

/// Answers every question with `answer` (or cancels) and remembers it.
struct TestPrompter {
    answer: Option<String>,
    asked: Mutex<Vec<AuthPrompt>>,
}

impl Prompter for TestPrompter {
    fn ask(&self, prompt: &AuthPrompt) -> Option<Vec<String>> {
        self.asked.lock().unwrap().push(prompt.clone());
        self.answer.clone().map(|a| vec![a; prompt.fields.len()])
    }
}

fn start_server(rt: &tokio::runtime::Runtime) -> u16 {
    rt.block_on(async {
        let config = Arc::new(russh::server::Config {
            keys: vec![russh::keys::decode_secret_key(HOST_KEY, None).unwrap()],
            auth_rejection_time: std::time::Duration::from_millis(10),
            auth_rejection_time_initial: Some(std::time::Duration::ZERO),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = Server.run_on_socket(config, &listener).await;
        });
        port
    })
}

fn profile(port: u16, auth: &str) -> ProfileInput {
    ProfileInput {
        id: None,
        name: format!("kbd {auth}"),
        kind: ProviderKind::Sftp,
        settings: [
            ("host".into(), "127.0.0.1".into()),
            ("port".into(), port.to_string()),
            ("user".into(), "me".into()),
            ("auth".into(), auth.into()),
        ]
        .into(),
        secrets: [("password".into(), PASSWORD.into())].into(),
    }
}

fn manager(dir: &tempfile::TempDir, answer: Option<&str>) -> (RemoteManager, Arc<TestPrompter>) {
    let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<MemorySecrets>::default());
    let prompter = Arc::new(TestPrompter {
        answer: answer.map(str::to_owned),
        asked: Mutex::default(),
    });
    m.set_prompter(prompter.clone());
    (m, prompter)
}

#[test]
fn keyboard_interactive_uses_the_saved_password_and_asks_for_the_rest() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = start_server(&rt);
    let dir = tempfile::tempdir().unwrap();

    // Both the explicit method and plain "password" (which the server
    // refuses, pointing to keyboard-interactive) get through.
    for auth in ["keyboard-interactive", "password"] {
        let (m, prompter) = manager(&dir, Some(CODE));
        let msg = m.test(&profile(port, auth)).unwrap_or_else(|e| panic!("{auth}: {e}"));
        assert!(msg.contains("2 items"), "{auth}: {msg}");
        let asked = prompter.asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "{auth}: only the code is asked, the password was saved");
        assert_eq!(asked[0].fields[0].prompt, "Verification code: ");
        assert!(asked[0].fields[0].echo);
        assert_eq!(asked[0].target, "me@127.0.0.1");
        assert!(asked[0].instructions.contains("authenticator"));
    }

    // The user gives up: the sign-in stops with a clear message.
    let (m, _) = manager(&dir, None);
    let err = m.test(&profile(port, "keyboard-interactive")).unwrap_err().to_string();
    assert!(err.contains("cancelled"), "{err}");

    // Nobody to ask (no UI): the error names the question.
    let m = RemoteManager::new(dir.path(), dir.path().join("cache"), Box::<MemorySecrets>::default());
    let err = m.test(&profile(port, "keyboard-interactive")).unwrap_err().to_string();
    assert!(err.contains("Verification code"), "{err}");
}

#[test]
fn a_wrong_saved_password_is_asked_for_again() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = start_server(&rt);
    let dir = tempfile::tempdir().unwrap();
    let (m, prompter) = manager(&dir, Some("wrong"));
    let mut input = profile(port, "keyboard-interactive");
    input.secrets.insert("password".into(), "not-it".into());
    assert!(m.test(&input).is_err());
    // The saved password is tried once; the server's next "Password:" goes to the user.
    let asked = prompter.asked.lock().unwrap();
    assert!(
        asked
            .iter()
            .any(|a| a.fields[0].prompt == "Password: " && !a.fields[0].echo),
        "{asked:?}"
    );
}
