#!/usr/bin/env bash
# NOT THE CANONICAL INSTALLER.
#
# The installer served at https://ferrosadb.com/install-memory.sh is published
# from the ferrosa repo's GitHub Pages site:
#
#     ferrosadb/ferrosa : docs/install-memory.sh
#
# This repo previously kept a hand-maintained copy here, which drifted from the
# served version. To avoid two sources of truth, edits must go to the ferrosa
# repo copy. This stub just redirects anyone who runs it.
echo "This is not the canonical installer. Run:" >&2
echo "  curl -fsSL https://ferrosadb.com/install-memory.sh | bash" >&2
echo "Source of truth: ferrosadb/ferrosa -> docs/install-memory.sh" >&2
exit 1
