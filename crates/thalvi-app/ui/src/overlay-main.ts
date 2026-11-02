import "./theme.css";
import "./overlay.css";
import "./module.css";
import { chooseBackend } from "./boot.ts";
import { mountOverlay, realClock } from "./overlay.ts";

const root = document.getElementById("overlay");
if (root) {
  void chooseBackend().then((backend) => {
    mountOverlay(root, backend, realClock);
  });
}
