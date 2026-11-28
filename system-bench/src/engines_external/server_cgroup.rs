use std::path::PathBuf;

use aqpbm_core::harness::cgroup::CgroupSource;

#[derive(clap::Args, Debug, Clone)]
pub struct ServerCgroupOptions {
    /// The server's cgroup v2 directory, e.g.
    /// `/sys/fs/cgroup/system.slice/docker-<id>.scope` (engines `clickhouse`
    /// and `victoriametrics`).
    #[arg(long)]
    pub server_cgroup_directory: Option<PathBuf>,
    /// The SSH destination of the server host, e.g. `bench@10.0.0.2`; the
    /// cgroup directory is read over SSH. Omitted: it is read on this host.
    #[arg(long)]
    pub server_ssh_host: Option<String>,
    /// The SSH client used with `--server-ssh-host`.
    #[arg(long, default_value = "ssh")]
    pub ssh_program: String,
}

impl ServerCgroupOptions {
    pub fn source(&self) -> Option<CgroupSource> {
        let cgroup_directory = self.server_cgroup_directory.clone()?;
        Some(match self.server_ssh_host.clone() {
            Some(host) => CgroupSource::Remote {
                ssh_program: self.ssh_program.clone(),
                host,
                cgroup_directory,
            },
            None => CgroupSource::Local(cgroup_directory),
        })
    }
}
