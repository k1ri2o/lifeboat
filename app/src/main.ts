import { invoke } from "@tauri-apps/api/core";

const el = document.getElementById("status")!;

async function refresh(): Promise<void> {
  try {
    el.textContent = await invoke<string>("service_status");
  } catch (e) {
    el.textContent = `Service not reachable: ${String(e)}`;
  }
}

void refresh();
setInterval(refresh, 5000);
