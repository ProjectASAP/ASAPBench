use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const LINUX_CPU_ROOT: &str = "/sys/devices/system/cpu";

#[derive(Debug, thiserror::Error)]
pub enum CpuFrequencyError {
    #[error("no CPU frequency governor under {0}: only Linux with cpufreq exposes one")]
    NoGovernor(PathBuf),
    #[error("{path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("governor {governor} is not available on {policy}; available: {available}")]
    Unavailable {
        governor: String,
        policy: PathBuf,
        available: String,
    },
    #[error("writing {path}: {source}{}", root_hint(.source))]
    Write { path: PathBuf, source: io::Error },
}

fn root_hint(source: &io::Error) -> &'static str {
    if source.kind() == io::ErrorKind::PermissionDenied {
        "; setting the governor needs root"
    } else {
        ""
    }
}

fn read_trimmed(path: &Path) -> Result<String, CpuFrequencyError> {
    fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .map_err(|source| CpuFrequencyError::Read {
            path: path.to_path_buf(),
            source,
        })
}

fn online(cpu: &Path) -> bool {
    fs::read_to_string(cpu.join("online")).map_or(true, |text| text.trim() != "0")
}

fn governor_files(cpu_root: &Path) -> Result<Vec<PathBuf>, CpuFrequencyError> {
    let Ok(entries) = fs::read_dir(cpu_root) else {
        return Err(CpuFrequencyError::NoGovernor(cpu_root.to_path_buf()));
    };
    let mut policies = BTreeSet::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let is_cpu = name
            .to_str()
            .and_then(|name| name.strip_prefix("cpu"))
            .is_some_and(|number| number.parse::<u32>().is_ok());
        let cpu = entry.path();
        if !is_cpu || !online(&cpu) {
            continue;
        }
        let Ok(policy) = fs::canonicalize(cpu.join("cpufreq")) else {
            continue;
        };
        if policy.join("scaling_governor").is_file() {
            policies.insert(policy);
        }
    }
    if policies.is_empty() {
        return Err(CpuFrequencyError::NoGovernor(cpu_root.to_path_buf()));
    }
    Ok(policies
        .into_iter()
        .map(|policy| policy.join("scaling_governor"))
        .collect())
}

pub fn governors(cpu_root: &Path) -> Result<BTreeSet<String>, CpuFrequencyError> {
    governor_files(cpu_root)?
        .iter()
        .map(|file| read_trimmed(file))
        .collect()
}

#[derive(Debug)]
pub struct HeldGovernor {
    previous: Vec<(PathBuf, String)>,
}

impl HeldGovernor {
    pub fn hold(cpu_root: &Path, governor: &str) -> Result<Self, CpuFrequencyError> {
        let files = governor_files(cpu_root)?;
        for file in &files {
            let available_file = file.with_file_name("scaling_available_governors");
            if !available_file.is_file() {
                continue;
            }
            let available = read_trimmed(&available_file)?;
            if !available.split_whitespace().any(|name| name == governor) {
                return Err(CpuFrequencyError::Unavailable {
                    governor: governor.to_string(),
                    policy: file.parent().map(Path::to_path_buf).unwrap_or_default(),
                    available,
                });
            }
        }
        let previous = files
            .into_iter()
            .map(|file| read_trimmed(&file).map(|value| (file, value)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut held = HeldGovernor {
            previous: Vec::with_capacity(previous.len()),
        };
        for (file, value) in previous {
            fs::write(&file, governor).map_err(|source| CpuFrequencyError::Write {
                path: file.clone(),
                source,
            })?;
            held.previous.push((file, value));
        }
        Ok(held)
    }

    pub fn restore_on_interrupt_or_panic(&self) {
        #[cfg(unix)]
        interrupt::arm(&self.previous);
    }

    pub fn restore(mut self) -> Result<(), CpuFrequencyError> {
        self.restore_all()
    }

    fn restore_all(&mut self) -> Result<(), CpuFrequencyError> {
        #[cfg(unix)]
        interrupt::disarm();
        let mut first_error = None;
        for (file, previous) in self.previous.drain(..) {
            if let Err(source) = fs::write(&file, &previous) {
                first_error.get_or_insert(CpuFrequencyError::Write { path: file, source });
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for HeldGovernor {
    fn drop(&mut self) {
        let _ = self.restore_all();
    }
}

#[cfg(unix)]
mod interrupt {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::Once;

    type Restores = Vec<(CString, CString)>;

    static ARMED: AtomicPtr<Restores> = AtomicPtr::new(std::ptr::null_mut());
    static INSTALL: Once = Once::new();
    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    fn write_back() {
        let restores = ARMED.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if restores.is_null() {
            return;
        }
        for (path, value) in unsafe { &*restores } {
            unsafe {
                let file = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_TRUNC);
                if file >= 0 {
                    libc::write(file, value.as_ptr().cast(), value.as_bytes().len());
                    libc::close(file);
                }
            }
        }
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        write_back();
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    pub(super) fn arm(previous: &[(PathBuf, String)]) {
        let restores: Restores = previous
            .iter()
            .filter_map(|(path, value)| {
                Some((
                    CString::new(path.as_os_str().as_bytes()).ok()?,
                    CString::new(value.as_str()).ok()?,
                ))
            })
            .collect();
        ARMED.store(Box::into_raw(Box::new(restores)), Ordering::SeqCst);
        INSTALL.call_once(|| {
            let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            for signal in SIGNALS {
                unsafe {
                    libc::signal(signal, handler);
                }
            }
            let previous_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                write_back();
                previous_hook(info);
            }));
        });
    }

    pub(super) fn disarm() {
        ARMED.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}
