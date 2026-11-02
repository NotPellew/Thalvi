import "./theme.css";
import "./settings.css";
import { chooseBackend } from "./boot.ts";
import { mountSettings } from "./settings.ts";

const root = document.getElementById("settings");
if (root) {
  void chooseBackend().then((backend) => {
    mountSettings(root, backend);
  });
}
