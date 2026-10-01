use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::KwinError;

const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_INTERFACE: &str = "org.kde.kwin.Scripting";
const KWIN_SCRIPT_INTERFACE: &str = "org.kde.kwin.Script";
const KWIN_CALLBACK_INTERFACE: &str = "org.kde.KWinMCP";
const SCRIPT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

static NEXT_SCRIPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn run_kwin_script(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
    script_body: &str,
) -> Result<String, KwinError> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
        KwinError::Msg(format!("KWin script requires a Tokio runtime: {error}"))
    })?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let pid = std::process::id();
    let sequence = NEXT_SCRIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let marker = format!("kwin-mcp-{pid}-{timestamp}-{sequence}");
    let callback_path = format!("/KWinMCP/{pid}_{timestamp}_{sequence}");
    let our_name = conn
        .unique_name()
        .ok_or_else(|| KwinError::Msg("no bus name".to_owned()))?
        .to_string();
    let our_name_json = serde_json::to_string(&our_name)?;
    let callback_path_json = serde_json::to_string(&callback_path)?;
    let script = format!(
        "{script_body}\n\
        callDBus({our_name_json},{callback_path_json},'{KWIN_CALLBACK_INTERFACE}','result',JSON.stringify(result));"
    );
    let object_path = zbus::zvariant::ObjectPath::try_from(callback_path.clone())?;
    let script_file = host_xdg_dir.join(format!("{marker}.js"));
    let mut cleanup = ScriptCleanup {
        conn: conn.clone(),
        kwin_unique: kwin_unique.to_owned(),
        object_path,
        marker,
        script_file,
        runtime,
        file_created: false,
        callback_attempted: false,
        load_attempted: false,
        cleanup_started: false,
    };

    let result = async {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&cleanup.script_file)?;
            cleanup.file_created = true;
            file.write_all(script.as_bytes())?;
        }
        let container_script_path = cleanup.script_file.to_string_lossy().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let callback = KWinCallback {
            tx: std::sync::Mutex::new(Some(tx)),
        };
        cleanup.callback_attempted = true;
        let registered = conn
            .object_server()
            .at(&cleanup.object_path, callback)
            .await?;
        eprintln!(
            "run_kwin_script: our_name={our_name} path={callback_path} registered={registered}"
        );
        if !registered {
            cleanup.callback_attempted = false;
            return Err(KwinError::Msg(format!(
                "failed to register callback at {callback_path}"
            )));
        }

        let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
            .destination(kwin_unique)?
            .path(KWIN_SCRIPTING_PATH)?
            .interface(KWIN_SCRIPTING_INTERFACE)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        cleanup.load_attempted = true;
        let (script_id,): (i32,) = scripting
            .call("loadScript", &(&container_script_path, &cleanup.marker))
            .await?;
        if script_id < 0 {
            return Err(KwinError::Msg(format!(
                "KWin loadScript failed, id={script_id}"
            )));
        }
        let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
            .destination(kwin_unique)?
            .path(format!("{KWIN_SCRIPTING_PATH}/Script{script_id}"))?
            .interface(KWIN_SCRIPT_INTERFACE)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        script_proxy.call::<_, (), ()>("run", &()).await?;
        rx.await
            .map_err(|_| KwinError::Msg("KWin callback channel closed".to_owned()))
    }
    .await;

    let cleanup_result = cleanup.finish().await;
    match result {
        Ok(payload) => {
            cleanup_result?;
            Ok(payload)
        }
        Err(error) => Err(error),
    }
}

struct ScriptCleanup {
    conn: zbus::Connection,
    kwin_unique: String,
    object_path: zbus::zvariant::ObjectPath<'static>,
    marker: String,
    script_file: PathBuf,
    runtime: tokio::runtime::Handle,
    file_created: bool,
    callback_attempted: bool,
    load_attempted: bool,
    cleanup_started: bool,
}

impl ScriptCleanup {
    fn start(&mut self) -> Option<tokio::task::JoinHandle<Result<(), KwinError>>> {
        if self.cleanup_started {
            return None;
        }
        self.cleanup_started = true;
        let file_result = if self.file_created {
            match std::fs::remove_file(&self.script_file) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    eprintln!(
                        "KWin script cleanup {}: remove {}: {error}",
                        self.marker,
                        self.script_file.display()
                    );
                    Err(KwinError::from(error))
                }
            }
        } else {
            Ok(())
        };
        let conn = self.conn.clone();
        let kwin_unique = self.kwin_unique.clone();
        let object_path = self.object_path.clone();
        let marker = self.marker.clone();
        let callback_attempted = self.callback_attempted;
        let load_attempted = self.load_attempted;

        Some(self.runtime.spawn(async move {
            let callback_cleanup = async {
                if !callback_attempted {
                    return Ok(());
                }
                match tokio::time::timeout(
                    SCRIPT_CLEANUP_TIMEOUT,
                    conn.object_server().remove::<KWinCallback, _>(&object_path),
                )
                .await
                {
                    Ok(Ok(_)) | Ok(Err(zbus::Error::InterfaceNotFound)) => Ok(()),
                    Ok(Err(error)) => Err(KwinError::from(error)),
                    Err(_) => Err(KwinError::Msg(format!(
                        "KWin callback cleanup timed out at {object_path} after {SCRIPT_CLEANUP_TIMEOUT:?}"
                    ))),
                }
            };
            let unload_cleanup = async {
                if !load_attempted {
                    return Ok(());
                }
                let reply = tokio::time::timeout(
                    SCRIPT_CLEANUP_TIMEOUT,
                    conn.call_method(
                        Some(kwin_unique.as_str()),
                        KWIN_SCRIPTING_PATH,
                        Some(KWIN_SCRIPTING_INTERFACE),
                        "unloadScript",
                        &(marker.as_str(),),
                    ),
                )
                .await
                .map_err(|_| {
                    KwinError::Msg(format!(
                        "KWin unloadScript cleanup timed out for {marker} after {SCRIPT_CLEANUP_TIMEOUT:?}"
                    ))
                })??;
                let (_,): (bool,) = reply.body().deserialize()?;
                Ok(())
            };
            let (callback_result, unload_result) =
                tokio::join!(callback_cleanup, unload_cleanup);
            if let Err(error) = &callback_result {
                eprintln!("KWin script cleanup {marker}: {error}");
            }
            if let Err(error) = &unload_result {
                eprintln!("KWin script cleanup {marker}: {error}");
            }
            file_result.and(callback_result).and(unload_result)
        }))
    }

    async fn finish(&mut self) -> Result<(), KwinError> {
        match self.start() {
            Some(task) => task.await.map_err(|error| {
                KwinError::Msg(format!("KWin script cleanup task failed: {error}"))
            })?,
            None => Ok(()),
        }
    }
}

impl Drop for ScriptCleanup {
    fn drop(&mut self) {
        if let Some(task) = self.start() {
            drop(task);
        }
    }
}

struct KWinCallback {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

#[zbus::interface(name = "org.kde.KWinMCP")]
impl KWinCallback {
    #[zbus(name = "result")]
    fn result(&self, payload: String) {
        match self.tx.lock() {
            Ok(mut sender) => {
                if let Some(tx) = sender.take()
                    && tx.send(payload).is_err()
                {
                    eprintln!("KWin callback receiver dropped");
                }
            }
            Err(error) => eprintln!("KWin callback lock poisoned: {error}"),
        }
    }
}
