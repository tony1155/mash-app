# headless 交付物

- `mash-cli.exe`：`feat/headless-cli` 分支的 release 构建（cargo build --release --bin mash-cli，对应源码见 src/cli.rs）。
- 放进 Mash 安装目录（与 Mash.exe 同级）即可使用；接口契约见集成仓库 docs/02-mash-cli-spec.md。
- 该目录仅为交付便利存在，不参与构建；上游合并 headless CLI 后应删除。
