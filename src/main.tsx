import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import Overlay from "./Overlay";
import Pin from "./Pin";
import Ball from "./Ball";
import Clip from "./Clip";

// 同一份前端构建，按窗口 label 分流：overlay 窗 = 截图覆盖层，其余 = 配置窗。
let label = "main";
try {
  label = getCurrentWindow().label;
} catch {
  label = "main";
}

const Root =
  label === "overlay"
    ? Overlay
    : label === "ball"
      ? Ball
      : label === "clip"
        ? Clip
        : label.startsWith("pin-")
          ? Pin
          : App;

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
