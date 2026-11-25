//! Trusted tools have bounded, process-owned data authority without a guest VM.
//! Operation socket closure preserves the context; explicit close or kernel exit
//! revokes it. Accepted storage work still belongs to the service until terminal.
use super::{
    Result,
    process_owner::Owner,
    service_call::{Context, ToolScope},
};
use crate::store::{Access, Id, LocalStore, Session};
use playbit_api::abi::syscall::Error as HostError;
use std::{
    collections::BTreeMap,
    os::{fd::OwnedFd, unix::net::UnixStream},
};

const MAX_CONTEXTS: usize = 32;

fn tool_handle(context: Context, current: Id) -> Option<Id> {
    match context {
        Context::Tool { handle, incarnation } if incarnation == current => Some(handle),
        _ => None,
    }
}

#[derive(Clone)]
pub(super) struct Binding {
    pub account: Id,
    pub session: Option<Session>,
    pub files: std::sync::Arc<super::tool_file::Transfers>,
}
impl Binding {
    pub fn execute(
        &self,
        call: crate::runtime::host::DataCall,
        host: &mut super::data_host::DataHost,
    ) -> crate::runtime::host::HostResult<crate::runtime::host::DataResult> {
        self.files.execute(call, host)
    }
    pub fn delivery_volumes(&self, volume: Id) -> Vec<Id> {
        let mut volumes = vec![volume];
        if let Some(session) = &self.session
            && !volumes.contains(&session.replica_volume())
        {
            volumes.push(session.replica_volume());
        }
        volumes
    }
    pub fn access(&self) -> Access {
        self.session.as_ref().map_or_else(
            || Access::owner(self.account),
            |session| {
                Access::session_version(self.account, &session.id, &session.resolved_build_id)
            },
        )
    }
    pub fn check(&self, store: &LocalStore) -> crate::runtime::host::HostResult<()> {
        use crate::runtime::store_bridge::error;
        if store.account().id != self.account {
            return Err(HostError::StaleHandle);
        }
        if let Some(session) = &self.session {
            let current = store.session(&session.id).map_err(error)?;
            if current.root_volume != session.root_volume
                || current.shared_volume != session.shared_volume
                || current.resolved_build_id != session.resolved_build_id
            {
                return Err(HostError::StaleHandle);
            }
            store.admit_binding(&self.access(), session.root_volume).map_err(error)?;
        }
        Ok(())
    }
}
struct Record {
    binding: Binding,
    owner: Owner,
}
pub(super) struct Contexts {
    incarnation: Id,
    records: BTreeMap<Id, Record>,
    file_budget: std::sync::Arc<super::tool_file::Budget>,
}
impl Contexts {
    pub fn new(incarnation: Id) -> Self {
        Self { incarnation, records: BTreeMap::new(), file_budget: Default::default() }
    }
    pub fn open(
        &mut self,
        store: &LocalStore,
        scope: ToolScope,
        peer: &UnixStream,
    ) -> Result<Context> {
        if self.records.len() >= MAX_CONTEXTS {
            return Err("Tool context limit reached; close an existing context".into());
        }
        let session = match scope {
            ToolScope::Account => None,
            ToolScope::Session(id) => Some(store.session(&id)?),
            ToolScope::Volume(volume) | ToolScope::BrowserVolume(volume) => {
                if store.volume(volume)?.owner == store.account().id {
                    None
                } else {
                    let browser = matches!(scope, ToolScope::BrowserVolume(_));
                    let joined = store.sessions()?.into_iter().find(|session| {
                        session.replica_volume() == volume
                            && (!browser || session.shared_volume.is_none())
                            && store
                                .session_denial(&session.id)
                                .is_ok_and(|denial| denial.is_none())
                    });
                    if browser {
                        joined
                    } else {
                        Some(joined.ok_or("Tool volume requires an authorized joined session")?)
                    }
                }
            }
        };
        let binding = Binding {
            account: store.account().id,
            session,
            files: std::sync::Arc::new(super::tool_file::Transfers::new(self.file_budget.clone())),
        };
        binding
            .check(store)
            .map_err(|error| format!("Tool context admission failed: {error:?}"))?;
        let owner = Owner::capture(peer)?;
        let handle = Id::generate()?;
        self.records.insert(handle, Record { binding, owner });
        Ok(Context::Tool { handle, incarnation: self.incarnation })
    }
    pub fn authorize(
        &self,
        context: Context,
        peer: &UnixStream,
    ) -> crate::runtime::host::HostResult<Binding> {
        let handle = tool_handle(context, self.incarnation).ok_or(HostError::StaleHandle)?;
        let record = self.records.get(&handle).ok_or(HostError::StaleHandle)?;
        let owner = Owner::capture(peer).map_err(|_| HostError::StaleHandle)?;
        if record.owner.identity != owner.identity {
            return Err(HostError::StaleHandle);
        }
        Ok(record.binding.clone())
    }
    pub fn close(
        &mut self,
        context: Context,
        peer: &UnixStream,
    ) -> crate::runtime::host::HostResult<()> {
        self.authorize(context, peer)?;
        self.records.remove(&context.handle());
        Ok(())
    }
    pub fn owns(&self, context: Context) -> bool {
        tool_handle(context, self.incarnation).is_some_and(|handle| self.owns_handle(handle))
    }
    pub fn owns_handle(&self, handle: Id) -> bool {
        self.records.contains_key(&handle)
    }
    pub fn delivery_volumes(&self, context: Context, volume: Id) -> Option<Vec<Id>> {
        let handle = tool_handle(context, self.incarnation)?;
        Some(self.records.get(&handle)?.binding.delivery_volumes(volume))
    }
    pub fn exits(&self) -> impl Iterator<Item = (Id, &OwnedFd)> {
        self.records.iter().map(|(handle, record)| (*handle, &record.owner.exit))
    }
    pub fn owner_exited(&mut self, handle: Id) {
        self.records.remove(&handle);
    }
    pub fn active(&self) -> bool {
        !self.records.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests use isolated stores and exact kernel-bound peers.")]
mod tests {
    use super::*;
    #[test]
    fn context_survives_normal_socket_close_and_explicit_close_revokes_it() {
        // Normal operation EOF preserves authority; a separate same-owner socket
        // can use it until explicit close, without a guest or session instance.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let epoch = Id::generate().unwrap();
        let mut contexts = Contexts::new(epoch);
        let (client, server) = UnixStream::pair().unwrap();
        let context = contexts.open(&store, ToolScope::Account, &server).unwrap();
        drop(client);
        drop(server);
        let (_client, server) = UnixStream::pair().unwrap();
        assert_eq!(contexts.authorize(context, &server).unwrap().account, store.account().id);
        let Context::Tool { handle, .. } = context else { panic!("tool context") };
        assert!(!contexts.owns(Context::Tool { handle, incarnation: Id::generate().unwrap() }));
        contexts.close(context, &server).unwrap();
        assert!(!contexts.owns(context));
        assert!(matches!(contexts.authorize(context, &server), Err(HostError::StaleHandle)));
        assert_eq!(contexts.exits().count(), 0);
    }
    #[test]
    fn capacity_and_engine_restart_do_not_reuse_tool_authority() {
        // Bound live contexts, reclaim explicitly closed capacity, and reject
        // every previous-incarnation context after a new engine starts.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let mut contexts = Contexts::new(Id::generate().unwrap());
        let (_client, peer) = UnixStream::pair().unwrap();
        let ids: Vec<_> = (0..MAX_CONTEXTS)
            .map(|_| contexts.open(&store, ToolScope::Account, &peer).unwrap())
            .collect();
        assert!(contexts.open(&store, ToolScope::Account, &peer).is_err());
        contexts.close(ids[0], &peer).unwrap();
        let fresh = contexts.open(&store, ToolScope::Account, &peer).unwrap();
        assert_ne!(fresh, ids[0]);
        let restarted = Contexts::new(Id::generate().unwrap());
        assert!(!restarted.owns(fresh));
        assert!(matches!(restarted.authorize(fresh, &peer), Err(HostError::StaleHandle)));
    }
    #[test]
    fn another_process_cannot_use_the_handle_and_kernel_exit_releases_it() {
        // Authenticate a child from its real socket, reject a parent using the
        // copied handle, then reclaim only after exact process-exit readiness.
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path().join("store")).unwrap();
        let path = directory.path().join("tool.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = Command::new("/usr/bin/python3")
            .args([
                "-c",
                "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); \
                 s.sendall(b'R'); s.recv(1); s.close(); sys.stdin.buffer.read(1)",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let mut ready = [0];
        peer.read_exact(&mut ready).unwrap();
        let mut contexts = Contexts::new(Id::generate().unwrap());
        let context = contexts.open(&store, ToolScope::Account, &peer).unwrap();
        peer.write_all(b"C").unwrap();
        assert_eq!(peer.read(&mut ready).unwrap(), 0);
        let (_client, parent) = UnixStream::pair().unwrap();
        assert!(matches!(contexts.authorize(context, &parent), Err(HostError::StaleHandle)));
        assert!(contexts.owns(context));
        child.stdin.take();
        let (handle, exit) = contexts.exits().next().unwrap();
        assert!(
            crate::engine::readiness::wait_readable(exit, Instant::now() + Duration::from_secs(5))
                .unwrap()
        );
        child.wait().unwrap();
        contexts.owner_exited(handle);
        assert!(!contexts.owns(context));
    }
}
