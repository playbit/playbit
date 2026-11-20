# Local store locking

Native LocalStore uses this order: optional personal-consent gate → sorted,
unique volume-operation locks → optional blob-cache gate → short profile
mutation lock → database-handle cache → database transaction. The volume-lock
registry mutex is released before acquiring any volume-operation lock. Guards
are never upgraded, and no network operation runs under these guards.

An owner operation locks its target volume. A session operation locks up to
three dependencies: its target and root volumes, plus its explicit session gate. It also holds shared
personal consent when the target is a selected personal volume. Session identity
is discovered before locking; authorization, version, denial and revocation are
reread after every dependency is locked. Edits retain these guards through the
read, transaction, commit and result filtering. Independent dependency sets can
commit concurrently; operations sharing a dependency observe complete
predecessors.

Metadata and revocation changes acquire their volume dependencies too. Session
adoption locks its root before the profile mutation; recording
an admission denial locks the session identity before the profile mutation.
Personal-volume selection takes exclusive consent before the profile lock.
Account rebinding additionally requires exclusive Rust ownership
(`&mut LocalStore`). Internal locked helpers do not reacquire public operation
locks, and replica callbacks must not reenter a public locking operation.

The registry retains idle locks across alternating working sets. It admits at
most 256 entries, preflights all missing dependencies and evicts only unborrowed,
unrequested entries when capacity is needed. Active and waiting operations
retain their Arc, preventing duplicate locks for the same volume. Poisoned locks
are reported as corruption. The database cache retains at most 32 handles with
512 KiB of redb page cache each (16 MiB total); active handles cannot be evicted.

Every native replica uses the shared `kvdb_store_redb::FileBackend`, whose
synchronization operation is literal `fsync`. The traced store wrapper delegates
that operation synchronously. Each database commits independently: there is no
store-wide flush queue, batch leader, secondary completion condition variable
or flush-admission limit. The backend's synchronization descriptor refers to the
same open file as its read/write handle, including after a pathname is renamed
or replaced. Redb's exclusive file lock remains held until explicit backend
close, and failure cleanup releases it. A closed traced wrapper must not unlock
a subsequent independently opened handle.

Commit failure remains an error, and publication occurs only after a successful
commit. This does not change authorization, transaction atomicity, recovery ordering or live-feed
ownership. It does not establish bulk-I/O fairness or acceptable input latency.
Packaged qualification must measure these paths with the real Engine scheduler,
background work and supported reference devices.
