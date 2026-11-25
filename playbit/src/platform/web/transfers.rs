//! One transport pins an owned backing; no catalog or database is leaked.
use super::*;
use databases::Address;

pub(super) const BLIND_WORLD_PUSH: u32 = 2;

// Drop order is part of the safety contract: the borrower is destroyed before
// its Rc owner. Fields are private to this worker module; no 'static borrow is
// exposed by any public API, and the stable Rc allocation is never mutated/moved.
pub(super) struct Sync {
    pub transfer: Transfer,
    pub database: Rc<Database>,
    pub address: Option<Address>,
    pub done: bool,
    pub account: bool,
    pub prepared: bool,
    pub(super) blind_world_push: bool,
}
#[allow(clippy::large_enum_variant, reason = "One bounded transport slot per worker.")]
pub(super) enum Transfer {
    Pull(PreparedSync<'static, OpfsStorage, OpfsStaging, OpfsStaging>),
    Push(SyncSender<'static, OpfsStorage>),
    Inspect(PreparedInspection<OpfsStaging>),
}
impl Sync {
    pub(super) fn authorize(&self) -> Result<()> {
        let Some(address) = &self.address else {
            return Ok(());
        };
        HOST.with(|cell| {
            let host = cell.borrow();
            let host = host.as_ref().ok_or(Error::Unavailable)?;
            if self.prepared {
                return publication::authorize_transfer(host, address);
            }
            let volume = host.replica_volume(address.volume)?;
            let projected = host.is_projected_world(&volume);
            if self.blind_world_push && !projected {
                return Err(Error::AccessDenied);
            }
            if matches!(self.transfer, Transfer::Push(_)) && projected {
                let DatabaseId::Entry(entry) = address.database else {
                    return Err(Error::AccessDenied);
                };
                let current = host.kv_backing(&volume, entry, Permission::Write)?;
                if !Rc::ptr_eq(&self.database, &current) {
                    return Err(Error::Conflict);
                }
                if self.blind_world_push {
                    let policy =
                        crate::store::DatabasePolicy::read(&volume.index, address.database)
                            .map_err(store_error)?;
                    return policy
                        .authorize_reader(crate::store::ReplicaReader::Service)
                        .map_err(store_error);
                }
            }
            let policy = host.replica_policy(&volume, address.database, true)?;
            policy.authorize_reader(crate::store::ReplicaReader::Service).map_err(store_error)
        })
    }
    pub(super) fn new(
        database: Rc<Database>,
        address: Option<Address>,
        direction: u32,
        output: &mut Vec<u8>,
    ) -> Result<Self> {
        if direction == BLIND_WORLD_PUSH && address.is_none() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: the owner moves into the result and outlives its private
        // transport field, including every error/unwind and early cancellation.
        let borrowed: &'static Database = unsafe { &*Rc::as_ptr(&database) };
        let transfer = match direction {
            0 => Transfer::Pull(
                PreparedSync::new(
                    borrowed,
                    OpfsStaging::new(0).map_err(map_error)?,
                    OpfsStaging::new(1).map_err(map_error)?,
                    64 * 1024 * 1024,
                    output,
                )
                .map_err(map_error)?,
            ),
            1 | BLIND_WORLD_PUSH => Transfer::Push(SyncSender::new(borrowed).map_err(map_error)?),
            _ => return Err(Error::InvalidArgument),
        };
        Ok(Self {
            transfer,
            database,
            account: address.is_none(),
            address,
            done: false,
            prepared: false,
            blind_world_push: direction == BLIND_WORLD_PUSH,
        })
    }
}
