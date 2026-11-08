# Concurrent tests json parity working-tree backup/restore race and atomic journal recovery

- **Learned from:** inner loop friction on CL I675c7334abab3dc5ec6183ac6ae11b3e3cfc8864, patchset 3 (task `src-performance-memory-batch-heapdump_vmo`)
- **Date:** 2026-10-30
- **Changed:** `checks/tests_json_parity.sh`

## Root cause

tests_json_parity.sh modified working-tree build files, <build_dir>/args.gn, and
.git/planter-tests-json-parity/{restore.json,backup} without an exclusive
process lock (fcntl.flock), and ran restore_from_journal() even on
PLANTER_SKIP_BUILD=1 runs before checking skip_build. When an overlapping or
backgrounded check run executed restore_from_journal() while another run was
mid-fx gen, it deleted backup_dir while the active run subsequently rewrote
args.gn and restore.json for the scoped pass; the active run's finally block
then unlinked args.gn before attempting shutil.copy2 from the deleted
backup_dir, failing with FileNotFoundError and leaving args.gn deleted.

## Why not a one-off fix

Manually restoring args.gn or deleting restore.json in a single checkout does
not prevent overlapping background and post-report check runs from racing on
.git/planter-tests-json-parity in future tasks. Serializing tests_json_parity.sh
via fcntl.flock, embedding inline base64 backups in restore.json, avoiding
pre-copy destination deletion, and reconstructing missing args.gn from args.json
eliminates the race and self-heals interrupted restores across all migrations.
