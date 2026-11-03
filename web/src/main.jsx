import React from "react";
import { createRoot } from "react-dom/client";

import "./purrimer/tokens.css";
import "./purrimer/bundle.css";
import "./app.css";
import { App } from "./App.jsx";

// Theme: Purrimer's greyscale pair only. The saved choice, else the OS preference
// (Blanc by day, Noir by night).
const savedTheme = localStorage.getItem("framevault-theme");
document.documentElement.dataset.theme = ["blanc", "noir"].includes(savedTheme)
  ? savedTheme
  : matchMedia("(prefers-color-scheme: dark)").matches
    ? "noir"
    : "blanc";

createRoot(document.getElementById("root")).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
