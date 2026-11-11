"""`tauri-runtime init` / `create` 的纯函数测试：文件修改（含幂等）、命令拼装与模板。"""

import json
import subprocess
import tomllib
from pathlib import Path

import pytest

from tauri_runtime import SDK_VERSION, cli
from tauri_runtime.cli import scaffold, templates
from tauri_runtime.cli.scaffold import Source

REPO = Path("/repo")

TAURI_CONF = json.dumps(
    {
        "productName": "demo",
        "build": {"beforeDevCommand": "pnpm dev", "beforeBuildCommand": "pnpm build"},
        "bundle": {"active": True, "icon": ["icons/icon.png"]},
    }
)

LIB_RS = """\
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
"""

BUILD_RS = """\
fn main() {
    tauri_build::build()
}
"""


def test_patch_tauri_conf_appends_build_step_and_resources():
    text, hints = scaffold.patch_tauri_conf(TAURI_CONF)
    conf = json.loads(text)
    assert hints == []
    assert conf["build"]["beforeBuildCommand"] == f"pnpm build && {scaffold.BUILD_STEP}"
    assert conf["bundle"]["resources"] == {"../src-python/dist/py-backend/": "py-backend/"}
    assert conf["bundle"]["icon"] == ["icons/icon.png"]
    assert scaffold.patch_tauri_conf(text) == (text, [])


def test_patch_tauri_conf_fills_empty_and_merges_map():
    conf = {"build": {"beforeBuildCommand": ""}, "bundle": {"resources": {"a/": "b/"}}}
    patched = json.loads(scaffold.patch_tauri_conf(json.dumps(conf))[0])
    assert patched["build"]["beforeBuildCommand"] == scaffold.BUILD_STEP
    assert patched["bundle"]["resources"] == {"a/": "b/", "../src-python/dist/py-backend/": "py-backend/"}


def test_patch_tauri_conf_hints_for_unsupported_forms():
    conf = {"build": {"beforeBuildCommand": {"script": "pnpm build"}}, "bundle": {"resources": ["a/*"]}}
    text, hints = scaffold.patch_tauri_conf(json.dumps(conf))
    assert json.loads(text) == conf
    assert len(hints) == 2


def test_patch_capability_is_idempotent():
    text = scaffold.patch_capability(json.dumps({"permissions": ["core:default"]}))
    assert json.loads(text)["permissions"] == ["core:default", "py-runtime:default"]
    assert scaffold.patch_capability(text) == text


def test_patch_lib_rs_inserts_after_builder():
    text = scaffold.patch_lib_rs(LIB_RS)
    assert text is not None
    lines = text.splitlines()
    assert lines[2] == "    tauri::Builder::default()"
    assert lines[3] == "        .plugin("
    assert lines[4] == "            tauri_plugin_py_runtime::Builder::new()"
    assert lines[8] == "        )"
    assert lines[9] == "        .plugin(tauri_plugin_opener::init())"
    assert scaffold.patch_lib_rs(text) == text


def test_patch_lib_rs_rejects_unrecognized_code():
    assert scaffold.patch_lib_rs("fn main() {}\n") is None
    assert scaffold.patch_lib_rs(LIB_RS + LIB_RS) is None
    assert scaffold.patch_lib_rs("let b = tauri::Builder::default();\n") is None


def test_patch_build_rs_creates_bundle_dir_first():
    text = scaffold.patch_build_rs(BUILD_RS)
    assert text is not None
    lines = text.splitlines()
    assert lines[2].startswith('    std::fs::create_dir_all("../src-python/dist/py-backend")')
    assert lines[3] == "    tauri_build::build()"
    assert scaffold.patch_build_rs(text) == text
    assert scaffold.patch_build_rs("fn main() {}\n") is None


def test_detect_package_manager(tmp_path):
    assert scaffold.detect_package_manager(tmp_path) == "npm"
    (tmp_path / "src-tauri").mkdir()
    (tmp_path / "src-tauri" / "tauri.conf.json").write_text(TAURI_CONF)
    assert scaffold.detect_package_manager(tmp_path) == "pnpm"
    (tmp_path / "bun.lock").write_text("")
    assert scaffold.detect_package_manager(tmp_path) == "bun"


def test_cargo_add_command():
    manifest = ["--manifest-path", str(Path("src-tauri") / "Cargo.toml")]
    assert scaffold.cargo_add_command(Source()) == ["cargo", "add", f"tauri-plugin-py-runtime@{SDK_VERSION}", *manifest]
    assert scaffold.cargo_add_command(Source("git")) == [
        "cargo", "add", "tauri-plugin-py-runtime", "--git", templates.REPO_URL, *manifest,
    ]
    assert scaffold.cargo_add_command(Source("path", REPO)) == [
        "cargo", "add", "tauri-plugin-py-runtime", "--path", str(REPO), *manifest,
    ]


def test_frontend_add_command():
    assert scaffold.frontend_add_command("npm", Source()) == ["npm", "install", f"tauri-plugin-py-runtime-api@{SDK_VERSION}"]
    assert scaffold.frontend_add_command("yarn", Source("git")) == ["yarn", "add", scaffold.GITHUB_SPEC]
    assert scaffold.frontend_add_command("pnpm", Source("git")) == [
        "pnpm", "add", "--allow-build=tauri-plugin-py-runtime-api", scaffold.GITHUB_SPEC,
    ]
    assert scaffold.frontend_add_command("pnpm", Source("path", REPO)) == ["pnpm", "add", f"link:{REPO}"]
    assert scaffold.frontend_add_command("bun", Source("path", REPO)) == ["bun", "add", f"file:{REPO}"]


def test_create_command():
    options = ["demo", "--template", "vue-ts", "--manager"]
    assert scaffold.create_command("demo", "vue-ts", "pnpm") == ["pnpm", "create", "tauri-app", *options, "pnpm", "--yes"]
    assert scaffold.create_command("demo", "vue-ts", "npm") == [
        "npm", "create", "tauri-app@latest", "--", *options, "npm", "--yes",
    ]


def test_pyproject_toml_per_source():
    registry = tomllib.loads(templates.pyproject_toml(Source()))
    assert registry["project"]["dependencies"] == ["tauri-runtime"]
    assert registry["dependency-groups"]["dev"] == ["nuitka"]
    assert registry["tool"]["tauri-runtime"]["build"]["name"] == "py-backend"
    assert "sources" not in registry["tool"]["uv"]

    git = tomllib.loads(templates.pyproject_toml(Source("git")))
    assert git["tool"]["uv"]["sources"]["tauri-runtime"] == {"git": templates.REPO_URL, "subdirectory": "python-sdk"}

    path = tomllib.loads(templates.pyproject_toml(Source("path", REPO)))
    assert path["tool"]["uv"]["sources"]["tauri-runtime"] == {"path": "/repo/python-sdk", "editable": True}


def test_main_py_template_compiles():
    compile(templates.MAIN_PY, "main.py", "exec")


TEMPLATE_LIB_RS = """\
// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
"""

TEMPLATE_APP_VUE = """\
<script setup lang="ts">
import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

const greetMsg = ref("");
const name = ref("");

async function greet() {
  // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
  greetMsg.value = await invoke("greet", { name: name.value });
}
</script>
"""

TEMPLATE_ANGULAR = """\
import { invoke } from "@tauri-apps/api/core";

export class AppComponent {
  greet(event: SubmitEvent, name: string): void {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    invoke<string>("greet", { name }).then((text) => {
      this.greetingMessage.set(text);
    });
  }
}
"""

TEMPLATE_VANILLA_JS = """\
const { invoke } = window.__TAURI__.core;

async function greet() {
  greetMsgEl.textContent = await invoke("greet", { name: greetInputEl.value });
}
"""


def test_patch_frontend_greet_switches_to_python():
    text = scaffold.patch_frontend_greet(TEMPLATE_APP_VUE)
    assert text is not None
    assert 'import { pyInvoke } from "tauri-plugin-py-runtime-api";' in text
    assert 'await pyInvoke("greet", { name: name.value });' in text
    assert "@tauri-apps/api/core" not in text
    assert "calling-rust" not in text
    assert scaffold.patch_frontend_greet(text) == text

    angular = scaffold.patch_frontend_greet(TEMPLATE_ANGULAR)
    assert angular is not None
    assert 'pyInvoke<string>("greet", { name })' in angular


def test_patch_frontend_greet_leaves_unsafe_code_alone():
    assert scaffold.patch_frontend_greet(TEMPLATE_VANILLA_JS) is None
    assert scaffold.patch_frontend_greet(TEMPLATE_APP_VUE + 'invoke("other");\n') is None
    assert scaffold.patch_frontend_greet("const x = 1;\n") == "const x = 1;\n"


def test_patch_rust_greet_removes_template_command():
    text = scaffold.patch_rust_greet(TEMPLATE_LIB_RS)
    assert text is not None
    assert "fn greet" not in text
    assert "invoke_handler" not in text
    assert "calling-rust" not in text
    assert text.startswith("#[cfg_attr(mobile, tauri::mobile_entry_point)]\npub fn run() {\n")
    assert "        .plugin(tauri_plugin_opener::init())\n        .run(" in text
    assert scaffold.patch_rust_greet(text) is None
    assert scaffold.patch_rust_greet(LIB_RS) is None


def make_tauri_project(root: Path, frontend: dict[str, str] | None = None, lib_rs: str = LIB_RS) -> Path:
    """最小 create-tauri-app 项目：配置、capability、lib.rs、package.json 与可选的前端文件。"""
    (root / "src-tauri" / "capabilities").mkdir(parents=True)
    (root / "src-tauri" / "src").mkdir()
    (root / "src-tauri" / "tauri.conf.json").write_text(TAURI_CONF)
    (root / "src-tauri" / "capabilities" / "default.json").write_text(json.dumps({"permissions": ["core:default"]}))
    (root / "src-tauri" / "src" / "lib.rs").write_text(lib_rs)
    (root / "src-tauri" / "build.rs").write_text(BUILD_RS)
    (root / "package.json").write_text("{}")
    for relative, content in (frontend or {}).items():
        (root / relative).parent.mkdir(parents=True, exist_ok=True)
        (root / relative).write_text(content)
    return root


class FakeRun:
    """记录外部命令（不含程序路径）；`create-tauri-app` 时生成带 Rust greet 示例的模板项目，可指定失败的程序。"""

    def __init__(self, fail: str | None = None, frontend: dict[str, str] | None = None):
        self.calls: list[tuple[list[str], Path]] = []
        self.fail = fail
        self.frontend = {"src/App.vue": TEMPLATE_APP_VUE} if frontend is None else frontend

    def __call__(self, cmd, cwd, check):
        args = [Path(cmd[0]).name, *cmd[1:]]
        self.calls.append((args, cwd))
        if "tauri-app" in args:
            make_tauri_project(cwd / args[args.index("tauri-app") + 1], self.frontend, TEMPLATE_LIB_RS)
        return subprocess.CompletedProcess(cmd, 1 if args[0] == self.fail else 0)


@pytest.fixture(autouse=True)
def fake_which(monkeypatch):
    monkeypatch.setattr(scaffold.shutil, "which", lambda name: f"/bin/{name}")


def test_init_sets_up_project(tmp_path):
    project = make_tauri_project(tmp_path)
    run = FakeRun()
    assert scaffold.init(project, run=run) == []
    assert [args for args, _ in run.calls] == [
        ["uv", "sync"],
        scaffold.cargo_add_command(Source()),
        scaffold.frontend_add_command("pnpm", Source()),
    ]
    assert run.calls[0][1] == project / "src-python"
    assert (project / "src-python" / "main.py").read_text() == templates.MAIN_PY
    assert "tauri_plugin_py_runtime" in (project / "src-tauri" / "src" / "lib.rs").read_text()
    assert templates.BUNDLE_DIR in (project / "src-tauri" / "build.rs").read_text()
    capability = json.loads((project / "src-tauri" / "capabilities" / "default.json").read_text())
    assert "py-runtime:default" in capability["permissions"]
    conf = json.loads((project / "src-tauri" / "tauri.conf.json").read_text())
    assert scaffold.BUILD_STEP in conf["build"]["beforeBuildCommand"]
    settings = json.loads((project / ".vscode" / "settings.json").read_text())
    assert settings == {scaffold.INTERPRETER_SETTING: scaffold.venv_interpreter()}
    extensions = json.loads((project / ".vscode" / "extensions.json").read_text())
    assert extensions == {"recommendations": [scaffold.PYTHON_EXTENSION]}


def test_venv_interpreter_per_platform():
    assert scaffold.venv_interpreter(windows=False) == "${workspaceFolder}/src-python/.venv/bin/python"
    assert scaffold.venv_interpreter(windows=True) == "${workspaceFolder}/src-python/.venv/Scripts/python.exe"


def test_patch_vscode_settings_merges_and_respects_user_choice():
    text = scaffold.patch_vscode_settings('{\n  "svelte.enable-ts-plugin": true\n}\n', "/py")
    assert text is not None
    assert json.loads(text) == {"svelte.enable-ts-plugin": True, scaffold.INTERPRETER_SETTING: "/py"}
    mine = json.dumps({scaffold.INTERPRETER_SETTING: "/usr/bin/python3"})
    assert scaffold.patch_vscode_settings(mine, "/py") == mine
    assert scaffold.patch_vscode_settings('{\n  // comment\n  "a": 1\n}\n', "/py") is None
    assert scaffold.patch_vscode_settings("[]", "/py") is None


def test_patch_vscode_extensions_adds_python():
    template = '{\n  "recommendations": ["Vue.volar", "rust-lang.rust-analyzer"]\n}\n'
    text = scaffold.patch_vscode_extensions(template)
    assert text is not None
    assert json.loads(text)["recommendations"] == ["Vue.volar", "rust-lang.rust-analyzer", scaffold.PYTHON_EXTENSION]
    assert scaffold.patch_vscode_extensions(text) == text
    assert scaffold.patch_vscode_extensions('{"recommendations": "x"}') is None


def test_setup_vscode_hints_for_unparsable_settings(tmp_path):
    (tmp_path / ".vscode").mkdir()
    jsonc = '{\n  // mine\n  "editor.tabSize": 2\n}\n'
    (tmp_path / ".vscode" / "settings.json").write_text(jsonc)
    hints = scaffold.setup_vscode(tmp_path)
    assert len(hints) == 1
    assert hints[0].startswith(".vscode/settings.json:")
    assert (tmp_path / ".vscode" / "settings.json").read_text() == jsonc
    assert (tmp_path / ".vscode" / "extensions.json").is_file()


def test_init_refuses_existing_backend_unless_forced(tmp_path):
    project = make_tauri_project(tmp_path)
    (project / "src-python").mkdir()
    (project / "src-python" / "main.py").write_text("mine")
    with pytest.raises(scaffold.ScaffoldError, match="--force"):
        scaffold.init(project, run=FakeRun())
    scaffold.init(project, force=True, run=FakeRun())
    assert (project / "src-python" / "main.py").read_text() == "mine"
    assert (project / "src-python" / "pyproject.toml").is_file()


def test_init_hints_when_lib_rs_unrecognized(tmp_path):
    project = make_tauri_project(tmp_path)
    (project / "src-tauri" / "src" / "lib.rs").write_text("fn main() {}\n")
    hints = scaffold.init(project, run=FakeRun())
    assert len(hints) == 1
    assert "tauri_plugin_py_runtime::Builder" in hints[0]


def test_init_stops_on_failed_command(tmp_path):
    project = make_tauri_project(tmp_path)
    run = FakeRun(fail="cargo")
    with pytest.raises(scaffold.ScaffoldError, match="cargo add"):
        scaffold.init(project, run=run)
    assert len(run.calls) == 2


def test_init_requires_tauri_project_and_valid_path_source(tmp_path):
    with pytest.raises(scaffold.ScaffoldError, match="src-tauri"):
        scaffold.init(tmp_path, run=FakeRun())
    with pytest.raises(scaffold.ScaffoldError, match="checkout"):
        scaffold.init(tmp_path, Source("path", tmp_path), run=FakeRun())


def test_create_runs_create_tauri_app_then_init(tmp_path):
    run = FakeRun()
    project, hints = scaffold.create("demo", manager="bun", source=Source("git"), cwd=tmp_path, run=run)
    assert project == tmp_path / "demo"
    assert hints == []
    assert run.calls[0] == (scaffold.create_command("demo", "vue-ts", "bun"), tmp_path)
    assert run.calls[-1][0] == ["bun", "add", scaffold.GITHUB_SPEC]
    with pytest.raises(scaffold.ScaffoldError, match="already exists"):
        scaffold.create("demo", cwd=tmp_path, run=run)


def test_create_replaces_rust_greet_with_python(tmp_path):
    project, hints = scaffold.create("demo", cwd=tmp_path, run=FakeRun())
    assert hints == []
    lib_rs = (project / "src-tauri" / "src" / "lib.rs").read_text()
    assert "fn greet" not in lib_rs
    assert "invoke_handler" not in lib_rs
    assert "tauri_plugin_py_runtime::Builder" in lib_rs
    assert 'pyInvoke("greet"' in (project / "src" / "App.vue").read_text()


def test_create_keeps_rust_greet_when_frontend_cannot_switch(tmp_path):
    run = FakeRun(frontend={"src/main.js": TEMPLATE_VANILLA_JS})
    project, hints = scaffold.create("demo", template="vanilla", cwd=tmp_path, run=run)
    assert len(hints) == 1
    assert "src/main.js" in hints[0]
    assert "fn greet" in (project / "src-tauri" / "src" / "lib.rs").read_text()
    assert (project / "src" / "main.js").read_text() == TEMPLATE_VANILLA_JS


def test_init_leaves_existing_greet_alone(tmp_path):
    project = make_tauri_project(tmp_path, {"src/App.vue": TEMPLATE_APP_VUE}, TEMPLATE_LIB_RS)
    scaffold.init(project, run=FakeRun())
    assert "fn greet" in (project / "src-tauri" / "src" / "lib.rs").read_text()
    assert (project / "src" / "App.vue").read_text() == TEMPLATE_APP_VUE


def test_main_init_reports_errors(tmp_path, capsys):
    assert cli.main(["init", "--project", str(tmp_path)]) == 1
    assert "src-tauri" in capsys.readouterr().err
