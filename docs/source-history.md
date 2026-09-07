# Source history

The initial history retains the commits touching these porthole paths:

- `crates/capture-transfer/`
- `tools/capture-viewer-sdl/`

`git-filter-repo` retained those paths and removed unrelated history. A subsequent
ordinary move renamed the crate directory to `crates/jackstay`; `git log --follow`
continues through the move. Original authors, dates and messages remain attached
to the filtered commits. Their hashes change because the trees and parents change.

The [commit map](source-commit-map.tsv) maps original porthole commit IDs to the
filtered IDs. The final retained source change is porthole PR #109, Windows
control-plane gating. Root workspace configuration, CI, the standalone build and
this documentation were added after extraction. Desktop consent/integration tests
remain in porthole and are tracked by its #114.
