import App from "./App.svelte";
import "./tokens.css";
import { initEvents } from "./lib/store";

initEvents().catch(console.error);

const app = new App({
  target: document.getElementById("app")!,
});

export default app;
