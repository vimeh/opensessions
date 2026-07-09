use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::mux::{MuxProvider, MuxSessionInfo};
use crate::tmux_provider::{
    PaneScope, SocketCommandRunner, STASH_SESSION, StdCommandRunner, TmuxClient,
};

type SocketClientFactory = Arc<dyn Fn(&Path) -> Option<TmuxClient> + Send + Sync>;

#[derive(Clone)]
pub struct RemoteTmuxProvider {
    discovery_root: PathBuf,
    local_user: String,
    local_client: TmuxClient,
    socket_client_factory: SocketClientFactory,
}

struct RemoteClient {
    host_tag: String,
    client: TmuxClient,
}

impl RemoteTmuxProvider {
    pub fn new(discovery_root: impl Into<PathBuf>, local_user: impl Into<String>) -> Self {
        Self::with_clients(
            discovery_root,
            local_user,
            TmuxClient::new(Arc::new(StdCommandRunner::default())),
            Arc::new(|socket_path: &Path| tmux_client_for_socket(socket_path)),
        )
    }

    fn with_clients(
        discovery_root: impl Into<PathBuf>,
        local_user: impl Into<String>,
        local_client: TmuxClient,
        socket_client_factory: SocketClientFactory,
    ) -> Self {
        Self {
            discovery_root: discovery_root.into(),
            local_user: local_user.into(),
            local_client,
            socket_client_factory,
        }
    }

    fn discover_clients(&self) -> Vec<RemoteClient> {
        let prefix = format!("inner-tmux-{}-", self.local_user);
        let suffix = ".sock";
        let Ok(entries) = fs::read_dir(&self.discovery_root) else {
            return Vec::new();
        };

        let mut remotes = entries
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let file_name = file_name.to_str()?;
                let host_tag = file_name.strip_prefix(&prefix)?.strip_suffix(suffix)?;
                if host_tag.is_empty() || !host_tag.chars().all(is_host_tag_char) {
                    return None;
                }
                let client = (self.socket_client_factory)(&entry.path())?;
                Some(RemoteClient {
                    host_tag: host_tag.to_string(),
                    client,
                })
            })
            .collect::<Vec<_>>();
        remotes.sort_by(|a, b| a.host_tag.cmp(&b.host_tag));
        remotes
    }

    fn client_for_host(&self, host_tag: &str) -> Option<TmuxClient> {
        self.discover_clients()
            .into_iter()
            .find(|remote| remote.host_tag == host_tag)
            .map(|remote| remote.client)
    }

    fn switch_local_to_host_pane(&self, host_tag: &str, client_tty: Option<&str>) {
        let filter = format!("#{{==:#{{@remote-host}},{host_tag}}}");
        let output = self.local_client.run(&[
            "list-panes",
            "-a",
            "-f",
            &filter,
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_id}",
        ]);
        if !output.ok() {
            return;
        }

        let Some((pane_id, session_name, window_id)) = output.stdout.lines().find_map(parse_host_pane)
        else {
            return;
        };

        self.local_client.switch_client(&session_name, client_tty);
        self.local_client.select_window(&window_id);
        self.local_client.select_pane(&pane_id);
    }

    fn switch_remote_client(&self, host_tag: &str, session_name: &str) {
        let Some(client) = self.client_for_host(host_tag) else {
            return;
        };
        let output = client.run(&["list-clients", "-F", "#{client_tty}"]);
        if !output.ok() {
            return;
        }
        let Some(client_tty) = output.stdout.lines().find(|line| !line.is_empty()) else {
            return;
        };
        let target = exact_session_target(session_name);
        client.run(&["switch-client", "-c", client_tty, "-t", &target]);
    }
}

impl Default for RemoteTmuxProvider {
    fn default() -> Self {
        Self::new("/tmp", std::env::var("USER").unwrap_or_default())
    }
}

impl MuxProvider for RemoteTmuxProvider {
    fn name(&self) -> &str {
        "tmux-remote"
    }

    fn list_sessions(&self) -> Vec<MuxSessionInfo> {
        let mut sessions = Vec::new();
        for remote in self.discover_clients() {
            let Some(remote_sessions) = remote.client.try_list_sessions() else {
                continue;
            };
            sessions.extend(
                remote_sessions
                    .into_iter()
                    .filter(|session| session.name != STASH_SESSION)
                    .map(|session| MuxSessionInfo {
                        name: namespaced_session(&remote.host_tag, &session.name),
                        created_at: session.created_at,
                        dir: session.dir,
                        windows: session.window_count,
                    }),
            );
        }
        sessions
    }

    fn switch_session(&self, name: &str, client_tty: Option<&str>) {
        let Some((host_tag, session_name)) = split_namespaced_session(name) else {
            return;
        };
        self.switch_local_to_host_pane(host_tag, client_tty);
        self.switch_remote_client(host_tag, session_name);
    }

    fn get_current_session(&self) -> Option<String> {
        None
    }

    fn get_session_dir(&self, name: &str) -> String {
        let Some((host_tag, session_name)) = split_namespaced_session(name) else {
            return String::new();
        };
        let Some(client) = self.client_for_host(host_tag) else {
            return String::new();
        };
        let target = exact_session_target(session_name);
        let output = client.run(&["display-message", "-p", "-t", &target, "#{pane_current_path}"]);
        if output.ok() {
            output.stdout
        } else {
            String::new()
        }
    }

    fn get_pane_count(&self, name: &str) -> u32 {
        let Some((host_tag, session_name)) = split_namespaced_session(name) else {
            return 0;
        };
        let Some(client) = self.client_for_host(host_tag) else {
            return 0;
        };
        let target = exact_session_target(session_name);
        client
            .try_list_panes(PaneScope::Session(&target))
            .map(|panes| panes.len() as u32)
            .unwrap_or(0)
    }

    fn get_client_tty(&self) -> String {
        String::new()
    }

    fn create_session(&self, _name: Option<&str>, _dir: Option<&str>) {}

    fn kill_session(&self, name: &str) {
        let Some((host_tag, session_name)) = split_namespaced_session(name) else {
            return;
        };
        let Some(client) = self.client_for_host(host_tag) else {
            return;
        };
        let target = exact_session_target(session_name);
        client.run(&["kill-session", "-t", &target]);
    }

    fn setup_hooks(&self, _server_host: &str, _server_port: u16) {}

    fn cleanup_hooks(&self) {}

    fn get_all_pane_counts(&self) -> HashMap<String, u32> {
        let mut counts = HashMap::new();
        for remote in self.discover_clients() {
            let Some(panes) = remote.client.try_list_panes(PaneScope::All) else {
                continue;
            };
            for pane in panes {
                if pane.session_name == STASH_SESSION {
                    continue;
                }
                *counts
                    .entry(namespaced_session(&remote.host_tag, &pane.session_name))
                    .or_insert(0) += 1;
            }
        }
        counts
    }
}

fn tmux_client_for_socket(socket_path: &Path) -> Option<TmuxClient> {
    let socket_path = socket_path.to_str()?;
    Some(TmuxClient::new(Arc::new(SocketCommandRunner::new(
        "tmux",
        socket_path,
    ))))
}

fn split_namespaced_session(name: &str) -> Option<(&str, &str)> {
    let (host_tag, session_name) = name.split_once('/')?;
    (!host_tag.is_empty() && !session_name.is_empty()).then_some((host_tag, session_name))
}

fn namespaced_session(host_tag: &str, session_name: &str) -> String {
    format!("{host_tag}/{session_name}")
}

fn exact_session_target(session_name: &str) -> String {
    format!("={session_name}")
}

fn parse_host_pane(line: &str) -> Option<(String, String, String)> {
    let mut parts = line.splitn(3, '\t');
    let pane_id = parts.next()?.to_string();
    let session_name = parts.next()?.to_string();
    let window_id = parts.next()?.to_string();
    (!pane_id.is_empty() && !session_name.is_empty() && !window_id.is_empty())
        .then_some((pane_id, session_name, window_id))
}

fn is_host_tag_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux_provider::{CommandOutput, CommandRunner};
    use parking_lot::Mutex;

    struct ScriptedRunner {
        calls: Mutex<Vec<Vec<String>>>,
        respond: Box<dyn Fn(&[String]) -> CommandOutput + Send + Sync>,
    }

    impl ScriptedRunner {
        fn new(respond: impl Fn(&[String]) -> CommandOutput + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                respond: Box::new(respond),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().clone()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().push(args.to_vec());
            (self.respond)(args)
        }
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn failed() -> CommandOutput {
        CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "no server running".to_string(),
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    /// Discovery root on disk, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "opensessions-remote-tmux-{label}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp discovery root");
            Self(dir)
        }

        fn touch(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, b"").expect("create socket placeholder");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Provider for user "vinay" whose remote sockets resolve to the given
    /// scripted runners. Discovery of any other socket path panics, so a
    /// filename that should have been rejected fails the test loudly.
    fn provider_with(
        root: &Path,
        local: Arc<ScriptedRunner>,
        remotes: Vec<(PathBuf, Arc<ScriptedRunner>)>,
    ) -> RemoteTmuxProvider {
        let remotes: HashMap<PathBuf, Arc<ScriptedRunner>> = remotes.into_iter().collect();
        RemoteTmuxProvider::with_clients(
            root,
            "vinay",
            TmuxClient::new(local),
            Arc::new(move |socket_path: &Path| {
                let runner = remotes.get(socket_path).unwrap_or_else(|| {
                    panic!(
                        "discovery considered unexpected socket {}",
                        socket_path.display()
                    )
                });
                Some(TmuxClient::new(runner.clone()))
            }),
        )
    }

    fn untouchable(side: &'static str) -> Arc<ScriptedRunner> {
        ScriptedRunner::new(move |args| panic!("{side} tmux must not be invoked: {args:?}"))
    }

    fn web1_pane_query() -> Vec<String> {
        argv(&[
            "list-panes",
            "-a",
            "-f",
            "#{==:#{@remote-host},web-1}",
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_id}",
        ])
    }

    fn remote_client_query() -> Vec<String> {
        argv(&["list-clients", "-F", "#{client_tty}"])
    }

    #[test]
    fn discovers_matching_sockets_and_namespaces_remote_sessions() {
        let dir = TempDir::new("discovery");
        let web_sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let db_sock = dir.touch("inner-tmux-vinay-db.host-2.sock");
        // None of these are remote sockets for user "vinay": wrong user,
        // empty host tag, invalid host tag character, wrong extension,
        // unrelated file.
        dir.touch("inner-tmux-root-web-1.sock");
        dir.touch("inner-tmux-vinay-.sock");
        dir.touch("inner-tmux-vinay-bad tag.sock");
        dir.touch("inner-tmux-vinay-web-1.sock.bak");
        dir.touch("notes.txt");

        let web = ScriptedRunner::new(|_| {
            ok("$1\tmain\t100\t1\t2\t/home/vinay/proj\n$2\t_os_stash\t50\t0\t1\t/tmp")
        });
        let db = ScriptedRunner::new(|_| ok("$1\talpha\t200\t0\t1\t/srv"));
        let provider = provider_with(
            &dir.0,
            untouchable("local"),
            vec![(web_sock, web), (db_sock, db)],
        );

        assert_eq!(
            provider.list_sessions(),
            vec![
                MuxSessionInfo {
                    name: "db.host-2/alpha".to_string(),
                    created_at: 200,
                    dir: "/srv".to_string(),
                    windows: 1,
                },
                MuxSessionInfo {
                    name: "web-1/main".to_string(),
                    created_at: 100,
                    dir: "/home/vinay/proj".to_string(),
                    windows: 2,
                },
            ],
        );
    }

    #[test]
    fn skips_unresponsive_sockets_silently() {
        let dir = TempDir::new("dead-socket");
        let live_sock = dir.touch("inner-tmux-vinay-live.sock");
        let dead_sock = dir.touch("inner-tmux-vinay-dead.sock");

        let live = ScriptedRunner::new(|_| ok("$1\tmain\t100\t1\t1\t/srv"));
        let dead = ScriptedRunner::new(|_| failed());
        let provider = provider_with(
            &dir.0,
            untouchable("local"),
            vec![(live_sock, live), (dead_sock, dead)],
        );

        let names = provider
            .list_sessions()
            .into_iter()
            .map(|session| session.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["live/main".to_string()]);
    }

    #[test]
    fn missing_discovery_root_means_no_remote_sessions() {
        let provider = provider_with(
            Path::new("/nonexistent/opensessions-discovery"),
            untouchable("local"),
            Vec::new(),
        );

        assert!(provider.list_sessions().is_empty());
    }

    #[test]
    fn switch_session_switches_local_pane_and_remote_client() {
        let dir = TempDir::new("switch-both");
        let sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let local = ScriptedRunner::new(|args| match args[0].as_str() {
            "list-panes" => ok("%5\tagents\t@2"),
            _ => ok(""),
        });
        let remote = ScriptedRunner::new(|args| match args[0].as_str() {
            "list-clients" => ok("/dev/pts/7"),
            _ => ok(""),
        });
        let provider = provider_with(&dir.0, local.clone(), vec![(sock, remote.clone())]);

        provider.switch_session("web-1/main", Some("/dev/ttys002"));

        assert_eq!(
            local.calls(),
            vec![
                web1_pane_query(),
                argv(&["switch-client", "-c", "/dev/ttys002", "-t", "agents"]),
                argv(&["select-window", "-t", "@2"]),
                argv(&["select-pane", "-t", "%5"]),
            ],
        );
        assert_eq!(
            remote.calls(),
            vec![
                remote_client_query(),
                argv(&["switch-client", "-c", "/dev/pts/7", "-t", "=main"]),
            ],
        );
    }

    #[test]
    fn switch_session_without_client_tty_switches_without_client_flag() {
        let dir = TempDir::new("switch-no-tty");
        let sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let local = ScriptedRunner::new(|args| match args[0].as_str() {
            "list-panes" => ok("%5\tagents\t@2"),
            _ => ok(""),
        });
        let remote = ScriptedRunner::new(|args| match args[0].as_str() {
            "list-clients" => ok("/dev/pts/7"),
            _ => ok(""),
        });
        let provider = provider_with(&dir.0, local.clone(), vec![(sock, remote)]);

        provider.switch_session("web-1/main", None);

        assert_eq!(
            local.calls()[1],
            argv(&["switch-client", "-t", "agents"]),
        );
    }

    #[test]
    fn switch_session_still_switches_remote_when_no_local_pane_matches() {
        for local_response in [ok(""), failed()] {
            let dir = TempDir::new("switch-no-pane");
            let sock = dir.touch("inner-tmux-vinay-web-1.sock");
            let local = ScriptedRunner::new(move |args| {
                assert_eq!(args[0], "list-panes", "unexpected local command {args:?}");
                local_response.clone()
            });
            let remote = ScriptedRunner::new(|args| match args[0].as_str() {
                "list-clients" => ok("/dev/pts/7"),
                _ => ok(""),
            });
            let provider = provider_with(&dir.0, local.clone(), vec![(sock, remote.clone())]);

            provider.switch_session("web-1/main", Some("/dev/ttys002"));

            assert_eq!(local.calls(), vec![web1_pane_query()]);
            assert_eq!(
                remote.calls(),
                vec![
                    remote_client_query(),
                    argv(&["switch-client", "-c", "/dev/pts/7", "-t", "=main"]),
                ],
            );
        }
    }

    #[test]
    fn switch_session_still_switches_local_when_remote_has_no_client() {
        for remote_response in [ok(""), failed()] {
            let dir = TempDir::new("switch-no-remote-client");
            let sock = dir.touch("inner-tmux-vinay-web-1.sock");
            let local = ScriptedRunner::new(|args| match args[0].as_str() {
                "list-panes" => ok("%5\tagents\t@2"),
                _ => ok(""),
            });
            let remote = ScriptedRunner::new(move |args| {
                assert_eq!(args[0], "list-clients", "unexpected remote command {args:?}");
                remote_response.clone()
            });
            let provider = provider_with(&dir.0, local.clone(), vec![(sock, remote.clone())]);

            provider.switch_session("web-1/main", Some("/dev/ttys002"));

            assert_eq!(
                local.calls(),
                vec![
                    web1_pane_query(),
                    argv(&["switch-client", "-c", "/dev/ttys002", "-t", "agents"]),
                    argv(&["select-window", "-t", "@2"]),
                    argv(&["select-pane", "-t", "%5"]),
                ],
            );
            assert_eq!(remote.calls(), vec![remote_client_query()]);
        }
    }

    #[test]
    fn switch_session_with_unknown_host_touches_no_remote_socket() {
        let dir = TempDir::new("switch-unknown-host");
        let sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let local = ScriptedRunner::new(|_| ok(""));
        let remote = untouchable("remote");
        let provider = provider_with(&dir.0, local, vec![(sock, remote.clone())]);

        provider.switch_session("ghost/main", None);

        assert!(remote.calls().is_empty());
    }

    #[test]
    fn bare_local_names_never_touch_local_or_remote_tmux() {
        let dir = TempDir::new("bare-name");
        let sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let local = untouchable("local");
        let remote = untouchable("remote");
        let provider = provider_with(&dir.0, local.clone(), vec![(sock, remote.clone())]);

        provider.switch_session("agents", Some("/dev/ttys002"));
        provider.kill_session("agents");

        assert!(local.calls().is_empty());
        assert!(remote.calls().is_empty());
    }

    #[test]
    fn kill_session_targets_only_the_named_host_socket() {
        let dir = TempDir::new("kill-routing");
        let alpha_sock = dir.touch("inner-tmux-vinay-alpha.sock");
        let beta_sock = dir.touch("inner-tmux-vinay-beta.sock");
        let alpha = untouchable("alpha");
        let beta = ScriptedRunner::new(|_| ok(""));
        let provider = provider_with(
            &dir.0,
            untouchable("local"),
            vec![(alpha_sock, alpha.clone()), (beta_sock, beta.clone())],
        );

        provider.kill_session("beta/work");
        // The first slash is the namespace separator; the rest is the
        // remote session name verbatim.
        provider.kill_session("beta/work/sub");

        assert_eq!(
            beta.calls(),
            vec![
                argv(&["kill-session", "-t", "=work"]),
                argv(&["kill-session", "-t", "=work/sub"]),
            ],
        );
        assert!(alpha.calls().is_empty());
    }

    #[test]
    fn kill_session_ignores_malformed_and_unknown_namespaced_names() {
        let dir = TempDir::new("kill-malformed");
        let sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let remote = untouchable("remote");
        let provider = provider_with(&dir.0, untouchable("local"), vec![(sock, remote.clone())]);

        provider.kill_session("web-1/");
        provider.kill_session("/work");
        provider.kill_session("ghost/work");

        assert!(remote.calls().is_empty());
    }

    #[test]
    fn pane_counts_namespace_sessions_and_skip_stash_and_dead_hosts() {
        fn pane_line(id: &str, session: &str) -> String {
            format!(
                "{id}\t{session}\t@1\t0\t0\t1\t/dev/pts/1\t100\t/home\tzsh\ttitle\t80\t24\t0\t79"
            )
        }

        let dir = TempDir::new("pane-counts");
        let live_sock = dir.touch("inner-tmux-vinay-web-1.sock");
        let dead_sock = dir.touch("inner-tmux-vinay-dead.sock");
        let panes = [
            pane_line("%1", "main"),
            pane_line("%2", "main"),
            pane_line("%3", "_os_stash"),
        ]
        .join("\n");
        let live = ScriptedRunner::new(move |_| ok(&panes));
        let dead = ScriptedRunner::new(|_| failed());
        let provider = provider_with(
            &dir.0,
            untouchable("local"),
            vec![(live_sock, live), (dead_sock, dead)],
        );

        assert_eq!(
            provider.get_all_pane_counts(),
            HashMap::from([("web-1/main".to_string(), 2)]),
        );
    }
}
