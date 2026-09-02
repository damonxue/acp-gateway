import { render } from "preact";
import { App } from "./ui/App";
import "./styles.css";

const root = document.getElementById("root");
if (root) render(<App />, root);
