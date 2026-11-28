import "./theme.css";
import "./settings.css";
import "./look-neon.css";
import "./look-holo.css";
import "./look-gunmetal.css";
import "./settings-looks.css";
import { chooseBackend } from "./boot.ts";
import { mountSettings } from "./settings.ts";

const root = document.getElementById("settings");
if (root) {
  void chooseBackend().then((backend) => {
    mountSettings(root, backend);
  });
}
