import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { installDesktopFeel } from "./lib/desktopFeel";
import "./styles.css";

installDesktopFeel();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
