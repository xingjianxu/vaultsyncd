//! Native update progress that remains visible while the WebView is stopped.
use std::{io, sync::mpsc, thread};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{
            CLSID_ProgressDialog, IProgressDialog, PROGDLG_MARQUEEPROGRESS, PROGDLG_NOCANCEL,
            PROGDLG_NOMINIMIZE, PROGDLG_NOTIME,
        },
    },
    core::HSTRING,
};

type Snapshot = (String, String, Option<(u64, u64)>);
type Update = (Snapshot, mpsc::SyncSender<io::Result<()>>);

/// A Windows Shell progress dialog on its own COM thread. Dropping it closes the dialog.
/// No cancellation is offered: the offline maintenance transaction must finish or recover.
pub struct UpdateProgress {
    sender: Option<mpsc::Sender<Update>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl UpdateProgress {
    /// Open an activity bar with the current stage and version information.
    pub fn new(stage: &str, detail: &str) -> io::Result<Self> {
        let (sender, updates) = mpsc::channel::<Update>();
        let (ready, started) = mpsc::sync_channel(1);
        let initial = (stage.to_owned(), detail.to_owned(), None);
        let worker = thread::Builder::new()
            .name("vaultsyncd-update-progress".into())
            .spawn(move || {
                let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                    .ok()
                    .map_err(io::Error::other);
                if let Err(error) = result {
                    let _ = ready.send(Err(error));
                    return;
                }
                struct Apartment;
                impl Drop for Apartment {
                    fn drop(&mut self) {
                        unsafe { CoUninitialize() };
                    }
                }
                let _apartment = Apartment;
                let mut dialog = match Dialog::new() {
                    Ok(dialog) => dialog,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                let result = dialog.set(initial);
                let failed = result.is_err();
                if ready.send(result).is_err() || failed {
                    return;
                }
                while let Ok((snapshot, completed)) = updates.recv() {
                    let result = dialog.set(snapshot);
                    let failed = result.is_err();
                    let _ = completed.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        let progress = Self {
            sender: Some(sender),
            worker: Some(worker),
        };
        started
            .recv()
            .map_err(|_| io::Error::other("更新进度窗口启动失败"))??;
        Ok(progress)
    }
    /// Show a stage with no measurable total, using an animated activity bar.
    pub fn stage(&self, stage: &str, detail: &str) -> io::Result<()> {
        self.set((stage.to_owned(), detail.to_owned(), None))
    }
    /// Show actual received bytes. This measures downloading only, never installation.
    pub fn download(&self, version: &str, received: u64, total: u64) -> io::Result<()> {
        if total == 0 || received > total {
            return Err(io::Error::other("更新下载进度无效"));
        }
        let percent = u128::from(received) * 100 / u128::from(total);
        self.set((
            format!("正在下载更新… {percent}%"),
            format!("版本 {version} · {received} / {total} 字节"),
            Some((received, total)),
        ))
    }
    fn set(&self, snapshot: Snapshot) -> io::Result<()> {
        let (completed, result) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::other("更新进度窗口已关闭"))?
            .send((snapshot, completed))
            .map_err(|_| io::Error::other("更新进度窗口已关闭"))?;
        result
            .recv()
            .map_err(|_| io::Error::other("更新进度窗口失去响应"))?
    }
}
impl Drop for UpdateProgress {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Dialog {
    native: IProgressDialog,
    measurable: Option<bool>,
}
impl Dialog {
    fn new() -> io::Result<Self> {
        let native = unsafe { CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER) }
            .map_err(io::Error::other)?;
        Ok(Self {
            native,
            measurable: None,
        })
    }
    fn set(&mut self, (stage, detail, bytes): Snapshot) -> io::Result<()> {
        unsafe {
            // Shell owns painting and its message loop; no UI handle crosses threads.
            // Reopen only when switching between marquee and measurable downloading.
            if self.measurable != Some(bytes.is_some()) {
                if self.measurable.is_some() {
                    // Shell cannot restart a stopped instance. Each mode owns
                    // exactly one Start/Stop lifetime, including on errors.
                    *self = Self::new()?;
                }
                self.native
                    .SetTitle(&HSTRING::from("vaultsyncd 自动更新"))
                    .map_err(io::Error::other)?;
                let flags = PROGDLG_NOCANCEL
                    | PROGDLG_NOMINIMIZE
                    | PROGDLG_NOTIME
                    | if bytes.is_some() {
                        0
                    } else {
                        PROGDLG_MARQUEEPROGRESS
                    };
                self.native
                    .StartProgressDialog(None, None, flags, None)
                    .map_err(io::Error::other)?;
                self.measurable = Some(bytes.is_some());
            }
            self.native
                .SetLine(1, &HSTRING::from(stage), false, None)
                .map_err(io::Error::other)?;
            self.native
                .SetLine(2, &HSTRING::from(detail), false, None)
                .map_err(io::Error::other)?;
            if let Some((received, total)) = bytes {
                self.native
                    .SetProgress64(received, total)
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    }
}
impl Drop for Dialog {
    fn drop(&mut self) {
        if self.measurable.is_some() {
            let _ = unsafe { self.native.StopProgressDialog() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the isolated interactive Windows S180 progress runner"]
    fn native_update_progress() {
        let pause = || thread::sleep(std::time::Duration::from_secs(3));
        let progress = UpdateProgress::new("正在检查更新…", "当前版本 0.1.6").unwrap();
        assert!(progress.download("0.1.7", 0, 0).is_err());
        assert!(progress.download("0.1.7", 101, 100).is_err());
        // The percentage calculation must also handle counters above u64::MAX / 100.
        progress.download("0.1.7", u64::MAX / 4, u64::MAX).unwrap();
        progress.stage("正在检查更新…", "当前版本 0.1.6").unwrap();
        pause();
        progress.download("0.1.7", 25, 100).unwrap();
        pause();
        progress.download("0.1.7", 100, 100).unwrap();
        pause();
        progress.stage("正在校验并解压更新…", "版本 0.1.7").unwrap();
        pause();
        progress
            .stage("正在排空同步、备份并安装更新…", "目标版本 0.1.7")
            .unwrap();
        pause();
        drop(progress);
        pause();
        let recovery =
            UpdateProgress::new("更新启动失败，正在恢复旧程序与状态…", "目标版本 0.1.7").unwrap();
        pause();
        drop(recovery);
        pause();
    }
}
