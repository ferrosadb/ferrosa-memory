#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
venv_dir="${FERROSA_EVAL_CORPUS_VENV:-${repo_root}/.eval-corpus/.venv}"
python_bin="${venv_dir}/bin/python"

mkdir -p "$(dirname "${venv_dir}")"

if [[ ! -x "${python_bin}" ]]; then
  python3 -m venv "${venv_dir}"
  "${python_bin}" -m pip install --upgrade pip
fi

"${python_bin}" - <<'PY' >/dev/null 2>&1 || "${python_bin}" -m pip install pyarrow numpy
import pyarrow
import numpy
PY

exec "${python_bin}" "${repo_root}/scripts/run-official-evals.py" "$@"
