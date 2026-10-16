// the "Getting started" overview: shown once on Home at first
// run (until dismissed, a pref), and again from Home or the Help panel.
import { FolderSync, GitBranch, GitCompare, GitMerge, Keyboard } from "lucide-react";
import type { ReactNode } from "react";
import { shortcutLabel } from "../lib/keymap";
import { Modal } from "./Modal";
import "./Vcs.css";

interface Props {
  onClose: () => void;
}

function Card({ icon, title, children }: { icon: ReactNode; title: string; children: ReactNode }) {
  return (
    <section className="getting-started-card">
      <h3>
        {icon}
        {title}
      </h3>
      <p>{children}</p>
    </section>
  );
}

export function GettingStarted({ onClose }: Props) {
  const key = (id: string, fallback: string) => <kbd>{shortcutLabel(id) || fallback}</kbd>;
  return (
    <Modal
      title="Getting started with PCompare"
      onClose={onClose}
      wide
      footer={
        <button className="primary" onClick={onClose}>
          Got it
        </button>
      }
    >
      <p className="getting-started-intro">Five things worth knowing. Reopen this any time from Home or Help.</p>
      <div className="getting-started-cards">
        <Card icon={<GitCompare size={16} />} title="Compare files and folders">
          Choose two files or folders in Quick Compare on Home, or drop them anywhere on the window — PCompare chooses
          Text, Folder, Table, Picture or Hex Compare from what they are. Archives (zip, tar, 7z) open as folders.
        </Card>
        <Card icon={<GitMerge size={16} />} title="Merge, 2-way and 3-way">
          Text Merge combines two files into an output you edit; add a base (common ancestor) for a 3-way merge that
          resolves non-conflicting changes for you. Folder Merge does the same for whole trees.
        </Card>
        <Card icon={<FolderSync size={16} />} title="Sync folders">
          Folder Sync previews every copy and delete before it runs: update, mirror or merge two folders, local or on a
          remote (SFTP, S3, cloud drives) set up under Profiles.
        </Card>
        <Card icon={<GitBranch size={16} />} title="Revisions and version control">
          Type <code>git:&lt;path&gt;@HEAD~1</code> (or <code>hg:</code>, <code>svn:</code>) in any path field to
          compare a past revision. Settings → Source control makes PCompare git's difftool and mergetool.
        </Card>
        <Card icon={<Keyboard size={16} />} title="Shortcuts and help">
          {key("app.help", "F1")} opens help for the view in front, with its shortcuts. {key("app.newTab", "Ctrl+T")}{" "}
          opens a new tab and {key("app.settings", "Ctrl+,")} the settings, where every shortcut can be changed.
        </Card>
      </div>
    </Modal>
  );
}
