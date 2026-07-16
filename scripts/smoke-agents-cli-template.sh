#!/usr/bin/env bash
# Certifies packaging/agents-cli-template/ against the REAL google-agents-cli
# package (not a mock): installs it into a scratch venv, scaffolds a project
# from the template using its LOCAL path, and asserts the generated project
# actually carries Keel's resilience wiring. This is the certification the
# WS4 plan calls for before the template path
# (`agents-cli create my-agent -a MisterTK/keel/packaging/agents-cli-template`)
# ships in any docs.
#
# --- local@ semantics, verified against the installed package -------------
# The brief's assumed invocation was `-a local@<abs path> --skip-deps`. Before
# relying on it, this script grepped the installed
# google/agents/cli/scaffold/utils/remote_template.py for "local@" and read
# the branch: `parse_agent_spec()` returns None immediately for any spec
# starting with "local@" (it never becomes a RemoteTemplateSpec). The actual
# `local@` handling lives one layer up, in
# google/agents/cli/scaffold/commands/create.py's `create()`: it strip the
# `local@` prefix, resolves the path, copies it into a temp dir (excluding
# .git/.venv/etc via get_standard_ignore_patterns — NOT excluding
# `.template`, that exclusion happens later in copy_files), and then feeds it
# through the exact same "render the base template via cookiecutter, then
# copy_files() the local/remote tree verbatim on top (skipping .git and any
# `.template` dir, overwrite=True)" path that a real git-fetched remote
# template goes through. So the brief's assumed invocation form is correct
# as written — confirmed empirically below, not just by reading source.
#
# --skip-deps is real (a hidden click flag on `create`, see
# shared_template_options in commands/create.py) and does exactly what's
# needed here: it skips the `uv add` of the resolved base template's
# dependencies (add_base_template_dependencies -> _add_dependencies, which
# shells out to `uv add` and would otherwise touch the network / require uv
# installed). Our own agents-cli-manifest.yaml already repeats those deps
# (deep-merge replaces extra_dependencies wholesale), so --skip-deps loses
# nothing here.
#
# Working invocation (verified locally, see below):
#   agents-cli create smoke-agent \
#     -a "local@<abs path to packaging/agents-cli-template>" \
#     --skip-deps --auto-approve --skip-checks --prototype \
#     --skip-welcome --deployment-target none
#
# The extra flags beyond the brief's --skip-deps keep this hermetic and
# non-interactive in CI: --auto-approve/--skip-checks avoid any GCP
# credential probing, --prototype + --deployment-target none skip
# Terraform/CI-CD generation (irrelevant to what we're certifying), and
# --skip-welcome quiets the banner.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
template_dir="${repo_root}/packaging/agents-cli-template"

if [[ ! -d "${template_dir}" ]]; then
  echo "smoke-agents-cli-template: FAIL — template dir not found: ${template_dir}" >&2
  exit 1
fi

fail() {
  echo "smoke-agents-cli-template: FAIL — $*" >&2
  exit 1
}

scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/keel-agents-cli-smoke.XXXXXX")"
cleanup() {
  rm -rf "${scratch_dir}"
}
trap cleanup EXIT

echo "smoke-agents-cli-template: scratch dir ${scratch_dir}"

venv_dir="${scratch_dir}/venv"
python3 -m venv "${venv_dir}"
# shellcheck disable=SC1091
source "${venv_dir}/bin/activate"

echo "smoke-agents-cli-template: installing google-agents-cli..."
pip install --quiet --upgrade pip
pip install --quiet google-agents-cli

agents_cli_pkg_dir="$(python3 -c 'import google.agents.cli as m, pathlib; print(pathlib.Path(m.__file__).parent)')"
remote_template_py="${agents_cli_pkg_dir}/scaffold/utils/remote_template.py"

if [[ ! -f "${remote_template_py}" ]]; then
  fail "expected remote_template.py not found at ${remote_template_py} — google-agents-cli's internal layout changed; re-verify local@ semantics before trusting this script"
fi

echo "smoke-agents-cli-template: local@ branch in remote_template.py (informational — verify-before-relying):"
grep -n "local@" "${remote_template_py}" || fail "no 'local@' reference found in remote_template.py — agents-cli's local@ handling may have moved; re-verify before relying on the invocation below"

project_dir="${scratch_dir}/project"
mkdir -p "${project_dir}"

echo "smoke-agents-cli-template: scaffolding from local template..."
(
  cd "${project_dir}"
  agents-cli create smoke-agent \
    -a "local@${template_dir}" \
    --skip-deps \
    --auto-approve \
    --skip-checks \
    --prototype \
    --skip-welcome \
    --deployment-target none
)

generated="${project_dir}/smoke-agent"

[[ -d "${generated}" ]] || fail "expected generated project at ${generated}, found nothing"

# --- assertions -------------------------------------------------------------

[[ -f "${generated}/app/keel.toml" ]] || fail "app/keel.toml missing from generated project — the agent-dir keel.toml did not survive the remote-template overlay"

[[ -f "${generated}/agents-cli-manifest.yaml" ]] || fail "agents-cli-manifest.yaml missing from generated project"

pyproject="${generated}/pyproject.toml"
[[ -f "${pyproject}" ]] || fail "pyproject.toml missing from generated project"

for dep in "keelrun" "google-adk" "a2a-sdk" "aiohttp"; do
  grep -q "${dep}" "${pyproject}" || fail "pyproject.toml is missing a dependency on '${dep}' — got:\n$(cat "${pyproject}")"
done

env_example="${generated}/.env.example"
[[ -f "${env_example}" ]] || fail ".env.example missing from generated project"
grep -q "^KEEL_ENABLE=1$" "${env_example}" || fail ".env.example does not set KEEL_ENABLE=1 — got:\n$(cat "${env_example}")"

echo "smoke-agents-cli-template: PASS — app/keel.toml, agents-cli-manifest.yaml, pyproject.toml (keelrun + google-adk + a2a-sdk + aiohttp), and .env.example (KEEL_ENABLE=1) all present in the generated project."
