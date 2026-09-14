import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export interface UpdateProgress {
  status: "checking" | "available" | "downloading" | "installing" | "updated" | "no-update" | "error";
  message: string;
  downloaded?: number;
  total?: number;
}

/**
 * 检查并安装 GitHub Release 上的更新。
 * onProgress 用于驱动 UI 进度展示；安装完成后自动重启应用。
 */
export async function checkAndInstall(
  onProgress: (p: UpdateProgress) => void
): Promise<void> {
  onProgress({ status: "checking", message: "正在检查更新…" });
  let update;
  try {
    update = await check();
  } catch (e) {
    onProgress({
      status: "error",
      message: "检查更新失败：" + errMsg(e),
    });
    return;
  }

  if (!update) {
    onProgress({ status: "no-update", message: "已经是最新版本。" });
    return;
  }

  onProgress({
    status: "available",
    message: `发现新版本 ${update.version}`,
  });

  try {
    onProgress({ status: "downloading", message: "正在下载更新…" });
    await update.downloadAndInstall((event) => {
      if (event.event === "Progress") {
        onProgress({ status: "downloading", message: "正在下载更新…" });
      }
    });
    onProgress({ status: "installing", message: "正在安装更新…" });
  } catch (e) {
    onProgress({ status: "error", message: "更新失败：" + errMsg(e) });
    return;
  }

  onProgress({ status: "updated", message: "更新完成，正在重启…" });
  try {
    await relaunch();
  } catch (e) {
    onProgress({
      status: "error",
      message: "安装完成但重启失败，请手动重启：" + errMsg(e),
    });
  }
}

function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
