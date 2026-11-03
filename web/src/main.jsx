import React from "react";
import { createRoot } from "react-dom/client";

import "./purrimer/tokens.css";
import "./purrimer/bundle.css";
import "./app.css";
import purrimerBundleURL from "./purrimer/bundle.js?url";

// Purrimer's component bundle is a classic script that reads React from `window` and
// publishes its components on `window.Purrimer`.
window.React = React;

// Theme: Purrimer's greyscale pair only. The saved choice, else the OS preference
// (Blanc by day, Noir by night).
const savedTheme = localStorage.getItem("framevault-theme");
document.documentElement.dataset.theme = ["blanc", "noir"].includes(savedTheme)
  ? savedTheme
  : matchMedia("(prefers-color-scheme: dark)").matches
    ? "noir"
    : "blanc";

function loadScript(src) {
  return new Promise((resolve, reject) => {
    const s = document.createElement("script");
    s.src = src;
    s.onload = resolve;
    s.onerror = () => reject(new Error(`Couldn't load ${src}`));
    document.head.appendChild(s);
  });
}

await loadScript(purrimerBundleURL);
const { App } = await import("./App.jsx");
createRoot(document.getElementById("root")).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
