//! Opt this helper thread into version 6 common controls, without changing the
//! service executable's activation context or depending on a user profile.
use std::io::Write;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::ApplicationInstallationAndServicing::*;
use windows::core::PCWSTR;

pub struct Theme {
    context: HANDLE,
    cookie: usize,
    manifest: std::path::PathBuf,
}
impl Theme {
    pub fn activate() -> anyhow::Result<Self> {
        let manifest =
            std::env::temp_dir().join(format!("meshrmm-task-{}.manifest", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&manifest)?;
        file.write_all(br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="MeshRMM.BackgroundTasks" type="win32"/>
<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
</assembly>"#)?;
        drop(file);
        let path = super::wide(&*manifest.to_string_lossy());
        let result = unsafe {
            CreateActCtxW(&ACTCTXW {
                cbSize: std::mem::size_of::<ACTCTXW>() as u32,
                lpSource: PCWSTR(path.as_ptr()),
                ..Default::default()
            })
        };
        let context = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(manifest);
                return Err(e.into());
            }
        };
        let mut theme = Self {
            context,
            cookie: 0,
            manifest,
        };
        unsafe {
            ActivateActCtx(Some(context), &mut theme.cookie)?;
        }
        // The activation context has loaded the manifest; no file needs to
        // survive forced job cleanup when the remote session ends.
        let _ = std::fs::remove_file(&theme.manifest);
        Ok(theme)
    }
}
impl Drop for Theme {
    fn drop(&mut self) {
        unsafe {
            if self.cookie != 0 {
                let _ = DeactivateActCtx(0, self.cookie);
            }
            ReleaseActCtx(self.context);
        }
        let _ = std::fs::remove_file(&self.manifest);
    }
}
