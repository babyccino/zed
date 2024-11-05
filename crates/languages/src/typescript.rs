use anyhow::{anyhow, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncAppContext;
use http_client::github::{build_asset_url, AssetKind, GitHubLspBinaryVersion};
use language::{LanguageServerName, LanguageToolchainStore, LspAdapter, LspAdapterDelegate};
use lazy_static::lazy_static;
use lsp::{CodeActionKind, LanguageServerBinary};
use node_runtime::NodeRuntime;
use project::lsp_store::language_server_settings;
use project::ContextProviderWithTasks;
use regex::Regex;
use rope::Rope;
use serde_json::{json, Value};
use smol::{fs, io::BufReader, stream::StreamExt};
use std::{
    any::Any,
    borrow::Cow,
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use task::{TaskTemplate, TaskTemplates, VariableName};
use util::{fs::remove_matching, maybe, ResultExt};

pub(super) fn typescript_task_context() -> ContextProviderWithTasks {
    ContextProviderWithTasks::new(TaskTemplates(vec![
        TaskTemplate {
            label: "jest file test".to_owned(),
            command: "npx jest".to_owned(),
            args: vec![VariableName::File.template_value()],
            ..TaskTemplate::default()
        },
        TaskTemplate {
            label: "jest test $ZED_SYMBOL".to_owned(),
            command: "npx jest".to_owned(),
            args: vec![
                "--testNamePattern".into(),
                format!("\"{}\"", VariableName::Symbol.template_value()),
                VariableName::File.template_value(),
            ],
            tags: vec!["ts-test".into(), "js-test".into(), "tsx-test".into()],
            ..TaskTemplate::default()
        },
        TaskTemplate {
            label: "execute selection $ZED_SELECTED_TEXT".to_owned(),
            command: "node".to_owned(),
            args: vec![
                "-e".into(),
                format!("\"{}\"", VariableName::SelectedText.template_value()),
            ],
            ..TaskTemplate::default()
        },
    ]))
}

fn typescript_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

fn eslint_server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![
        "--max-old-space-size=8192".into(),
        server_path.into(),
        "--stdio".into(),
    ]
}

pub struct TypeScriptLspAdapter {
    node: NodeRuntime,
}

impl TypeScriptLspAdapter {
    const OLD_SERVER_PATH: &'static str = "node_modules/typescript-language-server/lib/cli.js";
    const NEW_SERVER_PATH: &'static str = "node_modules/typescript-language-server/lib/cli.mjs";
    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static("typescript-language-server");
    pub fn new(node: NodeRuntime) -> Self {
        TypeScriptLspAdapter { node }
    }
    async fn tsdk_path(adapter: &Arc<dyn LspAdapterDelegate>) -> &'static str {
        let is_yarn = adapter
            .read_text_file(PathBuf::from(".yarn/sdks/typescript/lib/typescript.js"))
            .await
            .is_ok();

        if is_yarn {
            ".yarn/sdks/typescript/lib"
        } else {
            "node_modules/typescript/lib"
        }
    }
}

struct TypeScriptVersions {
    typescript_version: String,
    server_version: String,
}

pub trait Stripper {
    fn strip_before(&self, c: char) -> Option<&str>;
    fn strip_until(&self, c: char) -> Option<&str>;
}
impl Stripper for str {
    fn strip_before(&self, c: char) -> Option<&str> {
        self.split_once(c).map(|pair| pair.0)
    }
    fn strip_until(&self, c: char) -> Option<&str> {
        self.split_once(c).map(|pair| pair.1)
    }
}

#[async_trait(?Send)]
impl LspAdapter for TypeScriptLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME.clone()
    }

    async fn fetch_latest_server_version(
        &self,
        _: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        Ok(Box::new(TypeScriptVersions {
            typescript_version: self.node.npm_package_latest_version("typescript").await?,
            server_version: self
                .node
                .npm_package_latest_version("typescript-language-server")
                .await?,
        }) as Box<_>)
    }

    async fn fetch_server_binary(
        &self,
        latest_version: Box<dyn 'static + Send + Any>,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let latest_version = latest_version.downcast::<TypeScriptVersions>().unwrap();
        let server_path = container_dir.join(Self::NEW_SERVER_PATH);
        let package_name = "typescript";

        let should_install_language_server = self
            .node
            .should_install_npm_package(
                package_name,
                &server_path,
                &container_dir,
                latest_version.typescript_version.as_str(),
            )
            .await;

        if should_install_language_server {
            self.node
                .npm_install_packages(
                    &container_dir,
                    &[
                        (package_name, latest_version.typescript_version.as_str()),
                        (
                            "typescript-language-server",
                            latest_version.server_version.as_str(),
                        ),
                    ],
                )
                .await?;
        }

        Ok(LanguageServerBinary {
            path: self.node.binary_path().await?,
            env: None,
            arguments: typescript_server_binary_arguments(&server_path),
        })
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_ts_server_binary(container_dir, &self.node).await
    }

    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::REFACTOR,
            CodeActionKind::REFACTOR_EXTRACT,
            CodeActionKind::SOURCE,
        ])
    }

    async fn label_for_completion(
        &self,
        item: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        use lsp::CompletionItemKind as Kind;
        let len = item.label.len();
        let grammar = language.grammar()?;
        let highlight_id = match item.kind? {
            Kind::CLASS | Kind::INTERFACE | Kind::ENUM => grammar.highlight_id_for_name("type"),
            Kind::CONSTRUCTOR => grammar.highlight_id_for_name("type"),
            Kind::CONSTANT => grammar.highlight_id_for_name("constant"),
            Kind::FUNCTION | Kind::METHOD => grammar.highlight_id_for_name("function"),
            Kind::PROPERTY | Kind::FIELD => grammar.highlight_id_for_name("property"),
            Kind::VARIABLE => grammar.highlight_id_for_name("variable"),
            _ => None,
        }?;

        let text = match &item.detail {
            Some(detail) => format!("{} {}", item.label, detail),
            None => item.label.clone(),
        };

        Some(language::CodeLabel {
            text,
            runs: vec![(0..len, highlight_id)],
            filter_range: 0..len,
        })
    }

    async fn label_for_resolved_completion(
        &self,
        completion: &lsp::CompletionItem,
        language: &Arc<language::Language>,
    ) -> Option<language::CodeLabel> {
        fn trim(str: &str) -> Cow<str> {
            lazy_static! {
                static ref REGEX: Regex = Regex::new(r"(\s*\n)+\s*").unwrap();
            }
            REGEX.replace_all(str, " ")
        }

        const INTERFACE: &str = "interface ";
        const CONSTANT: &str = "const ";
        const TYPE: &str = "type ";
        const FUNCTION: &str = "function ";
        const NEW: &str = "new ";
        const CLASS: &str = "class ";
        const CONSTRUCTOR: &str = "constructor ";
        const VAR: &str = "var ";
        const LET: &str = "let ";
        const ALIAS: &str = "(alias) ";
        const METHOD: &str = "(method) ";
        const PROPERTY: &str = "(property) ";
        const ENUM: &str = "enum ";

        let kind = completion.kind?;
        let scan = completion.detail.as_ref()?.as_str();
        let import_text = "Auto import from '";
        let (scan, import) = if let Some(stripped) = scan.strip_prefix(import_text) {
            (
                stripped.strip_until('\n')?,
                Some(stripped.strip_before('\'')?),
            )
        } else {
            (scan, None)
        };

        let scan = if let Some(stripped) = scan.strip_prefix("namespace") {
            stripped.strip_until('\n')?
        } else {
            scan
        };

        let (label, range, runs) = match kind {
            lsp::CompletionItemKind::CLASS => {
                if let Some(stripped) = scan.strip_prefix(CONSTRUCTOR) {
                    let name_end = stripped.find(|c| (c == '(') || (c == '<'))? + NEW.len();
                    let highlight_keyword = language.grammar()?.highlight_id_for_name("keyword")?;
                    let source = Rope::from(format!("function {stripped}"));
                    let mut runs = vec![(0..3, highlight_keyword)];
                    runs.append(&mut adjust_runs(
                        language
                            .highlight_text(&source, FUNCTION.len()..FUNCTION.len() + scan.len()),
                        NEW.len(),
                    ));

                    let str = format!("new {}", &stripped);
                    let owned_label = Cow::from(trim(&str).into_owned());
                    Some((owned_label, NEW.len()..name_end, Some(runs)))
                } else if let Some(stripped) = scan.strip_prefix(TYPE) {
                    let name_end = stripped.find(|c| (c == ' ') || (c == '<'))? + TYPE.len();
                    Some((trim(scan), TYPE.len()..name_end, None))
                } else if let Some(stripped) = scan.strip_prefix(CLASS) {
                    let name_end = stripped
                        .find(|c| (c == '<') || (c == ' ') || (c == '\n'))
                        .map(|i| i + CLASS.len())
                        .unwrap_or(scan.len());
                    let label = scan[name_end..]
                        .find('\n')
                        .map(|i| i + name_end)
                        .map(|label_end| &scan[..label_end])
                        .unwrap_or(scan);
                    Some((Cow::from(label), CLASS.len()..name_end, None))
                } else {
                    None
                }
            }
            lsp::CompletionItemKind::FUNCTION => {
                if scan.starts_with(INTERFACE) {
                    None
                } else if let Some(label) = scan.strip_prefix(FUNCTION) {
                    let name_end = label.find(|c| (c == '(') || (c == '<'))?;
                    let source = Rope::from(scan);
                    let runs = language.highlight_text(&source, FUNCTION.len()..scan.len());
                    Some((trim(label), 0..name_end, Some(runs)))
                } else {
                    None
                }
            }
            lsp::CompletionItemKind::VARIABLE => {
                let scan = scan.strip_prefix(ALIAS).unwrap_or(scan);

                if let Some(stripped) = scan.strip_prefix(INTERFACE) {
                    let name_end = stripped.find(|c| c == ' ' || c == '<')? + INTERFACE.len();
                    let label = scan[name_end..]
                        .rfind('}')
                        .map(|i| &scan[..i + name_end])
                        .unwrap_or(scan);
                    Some((trim(label), 0..name_end, None))
                } else if let Some(stripped) = scan.strip_prefix(TYPE) {
                    let name_end = stripped.find(|c| c == ' ' || c == '<')? + TYPE.len();
                    let label = scan[name_end..]
                        .rfind('}')
                        .map(|i| &scan[..i + name_end + 1])
                        .unwrap_or(scan);
                    Some((trim(label), TYPE.len()..name_end, None))
                } else if let Some(stripped) = scan.strip_prefix(NEW) {
                    let name_end = stripped.find(|c| (c == '(') || (c == '<'))? + NEW.len();
                    // includes `new `
                    let full_label = trim(
                        scan[name_end..]
                            .find('\n')
                            .map(|i| &scan[..i + name_end])
                            .unwrap_or(scan),
                    );
                    // to get proper syntax highlighting we have to remove the `new` and add `function`
                    let source = Rope::from(format!("function {}", &full_label[NEW.len()..]));
                    let highlight_keyword = language.grammar()?.highlight_id_for_name("keyword")?;
                    let mut runs = vec![(0..3, highlight_keyword)];
                    runs.append(&mut adjust_runs(
                        language.highlight_text(
                            &source,
                            FUNCTION.len()..FUNCTION.len() + full_label.len() - NEW.len(),
                        ),
                        NEW.len(),
                    ));
                    Some((full_label, NEW.len()..name_end, Some(runs)))
                } else if let Some(stripped) = scan.strip_prefix(CONSTANT) {
                    let name_end = stripped.find(':')?;
                    let source = Rope::from(scan);
                    let runs = language.highlight_text(&source, CONSTANT.len()..scan.len());
                    Some((trim(stripped), 0..name_end, Some(runs)))
                } else if scan.starts_with(VAR) || scan.starts_with(LET) {
                    let label = &scan[VAR.len()..];
                    let name_end = label.find(':')?;
                    let source = Rope::from(scan);
                    let runs = language.highlight_text(&source, VAR.len()..scan.len());
                    Some((trim(label), 0..name_end, Some(runs)))
                } else {
                    None
                }
            }
            lsp::CompletionItemKind::CONSTANT => {
                let label = scan.strip_prefix(CONSTANT)?;
                let name_end = label.find(':')?;
                let source = Rope::from(scan);
                let runs = language.highlight_text(&source, CONSTANT.len()..scan.len());
                Some((trim(label), 0..name_end, Some(runs)))
            }
            lsp::CompletionItemKind::METHOD => {
                let scan = scan.strip_prefix(METHOD)?.strip_until('.')?;
                let trimmed = trim(scan);
                let name_end = trimmed.find(|c| c == '(' || c == '<')?;
                let source = Rope::from(format!("function {}", trimmed.as_ref()));
                let runs = language
                    .highlight_text(&source, FUNCTION.len()..trimmed.len() + FUNCTION.len());
                Some((trimmed, 0..name_end, Some(runs)))
            }
            lsp::CompletionItemKind::PROPERTY | lsp::CompletionItemKind::FIELD => {
                let scan = scan.strip_prefix(PROPERTY)?.strip_until('.')?;
                let trimmed = trim(scan);
                let name_end = trimmed.find(':')?;
                let source = Rope::from(format!("let {}", trimmed.as_ref()));
                let runs = language.highlight_text(&source, 4..4 + trimmed.len());
                Some((trimmed, 0..name_end, Some(runs)))
            }
            lsp::CompletionItemKind::CONSTRUCTOR => None,
            lsp::CompletionItemKind::INTERFACE => {
                let name_end = scan
                    .strip_prefix(INTERFACE)?
                    .find(|c| c == ' ' || c == '<')?
                    + INTERFACE.len();
                let label = scan[name_end..]
                    .rfind('}')
                    .map(|i| &scan[..i + name_end + 1])
                    .unwrap_or(scan);
                Some((trim(label), INTERFACE.len()..name_end, None))
            }
            lsp::CompletionItemKind::ENUM => {
                let name_end = scan
                    .strip_prefix(ENUM)?
                    .find(|c| c == ' ' || c == '\n')
                    .map(|i| i + ENUM.len())
                    .unwrap_or(scan.len());
                Some((Cow::from(scan), ENUM.len()..name_end, None))
            }
            _ => None,
        }?;

        let runs = runs.unwrap_or_else(|| {
            let source = Rope::from(label.as_ref());
            language.highlight_text(&source, 0..label.len())
        });

        let text = match import {
            Some(import) => format!("{} {}", label.as_ref(), import),
            None => label.into_owned(),
        };
        Some(language::CodeLabel {
            text,
            runs,
            filter_range: range,
        })
    }

    async fn initialization_options(
        self: Arc<Self>,
        adapter: &Arc<dyn LspAdapterDelegate>,
    ) -> Result<Option<serde_json::Value>> {
        let tsdk_path = Self::tsdk_path(adapter).await;
        Ok(Some(json!({
            "provideFormatter": true,
            "hostInfo": "zed",
            "tsserver": {
                "path": tsdk_path,
            },
            "preferences": {
                "includeInlayParameterNameHints": "all",
                "includeInlayParameterNameHintsWhenArgumentMatchesName": true,
                "includeInlayFunctionParameterTypeHints": true,
                "includeInlayVariableTypeHints": true,
                "includeInlayVariableTypeHintsWhenTypeMatchesName": true,
                "includeInlayPropertyDeclarationTypeHints": true,
                "includeInlayFunctionLikeReturnTypeHints": true,
                "includeInlayEnumMemberValueHints": true,
            }
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Arc<dyn LanguageToolchainStore>,
        cx: &mut AsyncAppContext,
    ) -> Result<Value> {
        let override_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &Self::SERVER_NAME, cx)
                .and_then(|s| s.settings.clone())
        })?;
        if let Some(options) = override_options {
            return Ok(options);
        }
        Ok(json!({
            "completions": {
              "completeFunctionCalls": true
            }
        }))
    }

    fn language_ids(&self) -> HashMap<String, String> {
        HashMap::from_iter([
            ("TypeScript".into(), "typescript".into()),
            ("JavaScript".into(), "javascript".into()),
            ("TSX".into(), "typescriptreact".into()),
        ])
    }
}

async fn get_cached_ts_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    maybe!(async {
        let old_server_path = container_dir.join(TypeScriptLspAdapter::OLD_SERVER_PATH);
        let new_server_path = container_dir.join(TypeScriptLspAdapter::NEW_SERVER_PATH);
        if new_server_path.exists() {
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&new_server_path),
            })
        } else if old_server_path.exists() {
            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: typescript_server_binary_arguments(&old_server_path),
            })
        } else {
            Err(anyhow!(
                "missing executable in directory {:?}",
                container_dir
            ))
        }
    })
    .await
    .log_err()
}

pub struct EsLintLspAdapter {
    node: NodeRuntime,
}

impl EsLintLspAdapter {
    const CURRENT_VERSION: &'static str = "2.4.4";
    const CURRENT_VERSION_TAG_NAME: &'static str = "release/2.4.4";

    #[cfg(not(windows))]
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::TarGz;
    #[cfg(windows)]
    const GITHUB_ASSET_KIND: AssetKind = AssetKind::Zip;

    const SERVER_PATH: &'static str = "vscode-eslint/server/out/eslintServer.js";
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("eslint");

    const FLAT_CONFIG_FILE_NAMES: &'static [&'static str] =
        &["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs"];

    pub fn new(node: NodeRuntime) -> Self {
        EsLintLspAdapter { node }
    }

    fn build_destination_path(container_dir: &Path) -> PathBuf {
        container_dir.join(format!("vscode-eslint-{}", Self::CURRENT_VERSION))
    }
}

#[async_trait(?Send)]
impl LspAdapter for EsLintLspAdapter {
    fn code_action_kinds(&self) -> Option<Vec<CodeActionKind>> {
        Some(vec![
            CodeActionKind::QUICKFIX,
            CodeActionKind::new("source.fixAll.eslint"),
        ])
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Arc<dyn LanguageToolchainStore>,
        cx: &mut AsyncAppContext,
    ) -> Result<Value> {
        let workspace_root = delegate.worktree_root_path();

        let eslint_user_settings = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &Self::SERVER_NAME, cx)
                .and_then(|s| s.settings.clone())
                .unwrap_or_default()
        })?;

        let mut code_action_on_save = json!({
            // We enable this, but without also configuring `code_actions_on_format`
            // in the Zed configuration, it doesn't have an effect.
            "enable": true,
        });

        if let Some(code_action_settings) = eslint_user_settings
            .get("codeActionOnSave")
            .and_then(|settings| settings.as_object())
        {
            if let Some(enable) = code_action_settings.get("enable") {
                code_action_on_save["enable"] = enable.clone();
            }
            if let Some(mode) = code_action_settings.get("mode") {
                code_action_on_save["mode"] = mode.clone();
            }
            if let Some(rules) = code_action_settings.get("rules") {
                code_action_on_save["rules"] = rules.clone();
            }
        }

        let problems = eslint_user_settings
            .get("problems")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let rules_customizations = eslint_user_settings
            .get("rulesCustomizations")
            .cloned()
            .unwrap_or_else(|| json!([]));

        let node_path = eslint_user_settings.get("nodePath").unwrap_or(&Value::Null);
        let use_flat_config = Self::FLAT_CONFIG_FILE_NAMES
            .iter()
            .any(|file| workspace_root.join(file).is_file());

        Ok(json!({
            "": {
                "validate": "on",
                "rulesCustomizations": rules_customizations,
                "run": "onType",
                "nodePath": node_path,
                "workingDirectory": {"mode": "auto"},
                "workspaceFolder": {
                    "uri": workspace_root,
                    "name": workspace_root.file_name()
                        .unwrap_or(workspace_root.as_os_str()),
                },
                "problems": problems,
                "codeActionOnSave": code_action_on_save,
                "codeAction": {
                    "disableRuleComment": {
                        "enable": true,
                        "location": "separateLine",
                    },
                    "showDocumentation": {
                        "enable": true
                    }
                },
                "experimental": {
                    "useFlatConfig": use_flat_config,
                },
            }
        }))
    }

    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME.clone()
    }

    async fn fetch_latest_server_version(
        &self,
        _delegate: &dyn LspAdapterDelegate,
    ) -> Result<Box<dyn 'static + Send + Any>> {
        let url = build_asset_url(
            "microsoft/vscode-eslint",
            Self::CURRENT_VERSION_TAG_NAME,
            Self::GITHUB_ASSET_KIND,
        )?;

        Ok(Box::new(GitHubLspBinaryVersion {
            name: Self::CURRENT_VERSION.into(),
            url,
        }))
    }

    async fn fetch_server_binary(
        &self,
        version: Box<dyn 'static + Send + Any>,
        container_dir: PathBuf,
        delegate: &dyn LspAdapterDelegate,
    ) -> Result<LanguageServerBinary> {
        let version = version.downcast::<GitHubLspBinaryVersion>().unwrap();
        let destination_path = Self::build_destination_path(&container_dir);
        let server_path = destination_path.join(Self::SERVER_PATH);

        if fs::metadata(&server_path).await.is_err() {
            remove_matching(&container_dir, |entry| entry != destination_path).await;

            let mut response = delegate
                .http_client()
                .get(&version.url, Default::default(), true)
                .await
                .map_err(|err| anyhow!("error downloading release: {}", err))?;
            match Self::GITHUB_ASSET_KIND {
                AssetKind::TarGz => {
                    let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
                    let archive = Archive::new(decompressed_bytes);
                    archive.unpack(&destination_path).await?;
                }
                AssetKind::Zip => {
                    node_runtime::extract_zip(
                        &destination_path,
                        BufReader::new(response.body_mut()),
                    )
                    .await?;
                }
            }

            let mut dir = fs::read_dir(&destination_path).await?;
            let first = dir.next().await.ok_or(anyhow!("missing first file"))??;
            let repo_root = destination_path.join("vscode-eslint");
            fs::rename(first.path(), &repo_root).await?;

            #[cfg(target_os = "windows")]
            {
                handle_symlink(
                    repo_root.join("$shared"),
                    repo_root.join("client").join("src").join("shared"),
                )
                .await?;
                handle_symlink(
                    repo_root.join("$shared"),
                    repo_root.join("server").join("src").join("shared"),
                )
                .await?;
            }

            self.node
                .run_npm_subcommand(&repo_root, "install", &[])
                .await?;

            self.node
                .run_npm_subcommand(&repo_root, "run-script", &["compile"])
                .await?;
        }

        Ok(LanguageServerBinary {
            path: self.node.binary_path().await?,
            env: None,
            arguments: eslint_server_binary_arguments(&server_path),
        })
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        let server_path =
            Self::build_destination_path(&container_dir).join(EsLintLspAdapter::SERVER_PATH);
        Some(LanguageServerBinary {
            path: self.node.binary_path().await.ok()?,
            env: None,
            arguments: eslint_server_binary_arguments(&server_path),
        })
    }
}

fn adjust_runs(
    mut runs: Vec<(Range<usize>, HighlightId)>,
    delta: usize,
) -> Vec<(Range<usize>, HighlightId)> {
    for (range, _) in &mut runs {
        range.start += delta;
        range.end += delta;
    }
    runs
}

#[cfg(target_os = "windows")]
async fn handle_symlink(src_dir: PathBuf, dest_dir: PathBuf) -> Result<()> {
    if fs::metadata(&src_dir).await.is_err() {
        return Err(anyhow!("Directory {} not present.", src_dir.display()));
    }
    if fs::metadata(&dest_dir).await.is_ok() {
        fs::remove_file(&dest_dir).await?;
    }
    fs::create_dir_all(&dest_dir).await?;
    let mut entries = fs::read_dir(&src_dir).await?;
    while let Some(entry) = entries.try_next().await? {
        let entry_path = entry.path();
        let entry_name = entry.file_name();
        let dest_path = dest_dir.join(&entry_name);
        fs::copy(&entry_path, &dest_path).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::language;
    use crate::typescript::TypeScriptLspAdapter;
    use gpui::{Context, Hsla, TestAppContext};
    use language::{CodeLabel, LspAdapter};
    use lsp::{CompletionItem, CompletionItemKind};
    use node_runtime::FakeNodeRuntime;
    use unindent::Unindent;

    #[gpui::test]
    async fn test_get_completion_details() {
        let adapter = TypeScriptLspAdapter::new(FakeNodeRuntime::new());
        let language = language("typescript", tree_sitter_typescript::language_typescript());
        let theme = theme::SyntaxTheme::new_test([
            ("type", Hsla::default()),
            ("keyword", Hsla::default()),
            ("function", Hsla::default()),
            ("property", Hsla::default()),
            ("string", Hsla::default()),
        ]);
        language.set_theme(&theme);

        let grammar = language.grammar().unwrap();
        let highlight_type = grammar.highlight_id_for_name("type").unwrap();
        let highlight_keyword = grammar.highlight_id_for_name("keyword").unwrap();
        let highlight_generic = grammar.highlight_id_for_name("type").unwrap();
        let highlight_field = grammar.highlight_id_for_name("property").unwrap();

        let completion = CompletionItem {
            label: "foo".to_string(),
            detail: Some("var foo: string".to_string()),
            kind: Some(CompletionItemKind::VARIABLE),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            filter_range: 0..3,
            runs: vec![(5..11, highlight_type)],
            text: "foo: string".to_string(),
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "foo".to_string(),
            detail: Some("let foo: string".to_string()),
            kind: Some(CompletionItemKind::VARIABLE),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "foo: string".to_string(),
            runs: vec![(5..11, highlight_type)],
            filter_range: 0..3,
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "foo".to_string(),
            detail: Some("function foo()".to_string()),
            kind: Some(CompletionItemKind::FUNCTION),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "foo()".to_string(),
            filter_range: 0..3,
            runs: vec![],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "Foo".to_string(),
            detail: Some("interface Foo {}".to_string()),
            kind: Some(CompletionItemKind::INTERFACE),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "interface Foo {}".to_string(),
            filter_range: 10..13,
            runs: vec![(0..9, highlight_keyword), (10..13, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "Foo".to_string(),
            detail: Some("enum Foo".to_string()),
            kind: Some(CompletionItemKind::ENUM),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "enum Foo".to_string(),
            filter_range: 5..8,
            runs: vec![(0..4, highlight_keyword), (5..8, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "foo".to_string(),
            detail: Some("const foo: string".to_string()),
            kind: Some(CompletionItemKind::CONSTANT),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "foo: string".to_string(),
            filter_range: 0..3,
            runs: vec![(5..11, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "Hello".to_string(),
            detail: Some("constructor Hello(): Hello".to_string()),
            kind: Some(CompletionItemKind::CLASS),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "new Hello(): Hello".to_string(),
            filter_range: 4..9,
            runs: vec![
                (0..3, highlight_keyword),
                (4..9, highlight_type),
                (13..18, highlight_type),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "lchmodSync".to_string(),
            detail: Some(
                "Auto import from 'fs'\nfunction lchmodSync(path: PathLike, mode: Mode): void"
                    .to_string(),
            ),
            kind: Some(CompletionItemKind::FUNCTION),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "lchmodSync(path: PathLike, mode: Mode): void fs".to_string(),
            filter_range: 0..10,
            runs: vec![
                (17..25, highlight_type),
                (33..37, highlight_type),
                (40..44, highlight_keyword),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        // these fail for some reason. Bug with highlight_text?
        // let completion = CompletionItem {
        //     label: "localConst".to_string(),
        //     detail: Some("const localConst: \"\"".to_string()),
        //     kind: Some(CompletionItemKind::VARIABLE),
        //     ..Default::default()
        // };
        // let expected_label = CodeLabel {
        //     text: "localConst: \"\"".to_string(),
        //     filter_range: 0..10,
        //     runs: vec![(12..14, highlight_string)],
        // };
        // assert_eq!(
        //     adapter
        //         .label_for_resolved_completion(&completion, &language)
        //         .await
        //         .unwrap(),
        //     expected_label
        // );

        // let completion = CompletionItem {
        //     label: "localConst".to_string(),
        //     detail: Some("const localConst: 2".to_string()),
        //     kind: Some(CompletionItemKind::VARIABLE),
        //     ..Default::default()
        // };
        // let expected_label = CodeLabel {
        //     text: "localConst: 2".to_string(),
        //     filter_range: 0..10,
        //     runs: vec![(12..13, highlight_string)],
        // };
        // assert_eq!(
        //     adapter
        //         .label_for_resolved_completion(&completion, &language)
        //         .await
        //         .unwrap(),
        //     expected_label
        // );

        let completion = CompletionItem {
            label: "ModuleGenericClass".to_string(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some("Auto import from 'class-module'\nconstructor ModuleGenericClass<T = any>(hi: T): ModuleGenericClass<T>".to_string()),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "new ModuleGenericClass<T = any>(hi: T): ModuleGenericClass<T> class-module"
                .to_string(),
            filter_range: 4..22,
            runs: vec![
                (0..3, highlight_keyword),
                (4..22, highlight_type),
                (23..24, highlight_type),
                // (25..26, highlight_keyword),
                (27..30, highlight_type),
                (36..37, highlight_generic),
                (40..58, highlight_type),
                // (58..59, highlight_keyword),
                (59..60, highlight_generic),
                // (60..61, highlight_keyword),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "ModuleClass".to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(
                "(alias) new ModuleClass(hi: string): ModuleClass\nimport ModuleClass".to_string(),
            ),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "new ModuleClass(hi: string): ModuleClass".to_string(),
            filter_range: 4..15,
            runs: vec![
                (0..3, highlight_keyword),
                (4..15, highlight_type),
                (20..26, highlight_type),
                (29..40, highlight_type),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
          label: "Mock".to_string(),
          kind: Some(CompletionItemKind::VARIABLE),
          detail: Some("Auto import from 'node:test'\n(alias) type Mock<F extends Function> = F & {\n    mock: MockFunctionContext<F>;\n}\nexport Mock".to_string()),
          ..Default::default()
        };
        let label = adapter
            .label_for_resolved_completion(&completion, &language)
            .await
            .unwrap();
        assert_eq!(
            label.text,
            "type Mock<F extends Function> = F & { mock: MockFunctionContext<F>; } node:test"
        );
        assert_eq!(label.filter_range, 5..9);

        let completion = CompletionItem {
              label: "ModuleGenericClass".to_string(),
              kind: Some(CompletionItemKind::CLASS),
              detail: Some("Auto import from 'class-module'\nconstructor ModuleGenericClass<T = any>(hi: T): ModuleGenericClass<T>".to_string()),
              ..Default::default()
        };
        let label = adapter
            .label_for_resolved_completion(&completion, &language)
            .await
            .unwrap();
        assert_eq!(
            label.text,
            "new ModuleGenericClass<T = any>(hi: T): ModuleGenericClass<T> class-module"
        );
        assert_eq!(label.filter_range, 4..22);

        let completion = CompletionItem {
            label: "member".to_string(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("(property) ModuleClass.member: string".to_string()),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "member: string".to_string(),
            filter_range: 0..6,
            runs: vec![(8..14, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "method".to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some("(method) ModuleClass.method(hi: string): void".to_string()),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "method(hi: string): void".to_string(),
            filter_range: 0..6,
            runs: vec![(11..17, highlight_type), (20..24, highlight_keyword)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "Module".to_string(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(
                "Auto import from 'module'\nclass Module\ninterface Module\nnamespace Module"
                    .to_string(),
            ),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "class Module module".to_string(),
            filter_range: 6..12,
            runs: vec![(0..5, highlight_keyword), (6..12, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "Module".to_string(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some("class Module".to_string()),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "class Module".to_string(),
            filter_range: 6..12,
            runs: vec![(0..5, highlight_keyword), (6..12, highlight_type)],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
          label: "ModuleGenericType".to_string(),
          kind: Some(CompletionItemKind::CLASS),
          detail: Some("Auto import from 'type-module'\ntype ModuleGenericType<T = string> = {\n    hi: T;\n}".to_string()),
          ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "type ModuleGenericType<T = string> = { hi: T; } type-module".to_string(),
            filter_range: 5..22,
            runs: vec![
                (0..4, highlight_keyword),
                (5..22, highlight_type),
                // (22..23, highlight_keyword),
                (23..24, highlight_type),
                // (25..26, highlight_keyword),
                (27..33, highlight_type),
                // (33..34, highlight_keyword),
                // (35..36, highlight_keyword),
                (39..41, highlight_field),
                (43..44, highlight_type),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );

        let completion = CompletionItem {
            label: "member".to_string(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(
                "(property) LocalClass.member: {\n    hi: string;\n    there: string;\n}"
                    .to_string(),
            ),
            ..Default::default()
        };
        let expected_label = CodeLabel {
            text: "member: { hi: string; there: string; }".to_string(),
            filter_range: 0..6,
            runs: vec![
                (10..12, highlight_field),
                (14..20, highlight_type),
                (22..27, highlight_field),
                (29..35, highlight_type),
            ],
        };
        assert_eq!(
            adapter
                .label_for_resolved_completion(&completion, &language)
                .await
                .unwrap(),
            expected_label
        );
    }

    #[gpui::test]
    async fn test_outline(cx: &mut TestAppContext) {
        let language = crate::language(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        );

        let text = r#"
            function a() {
              // local variables are omitted
              let a1 = 1;
              // all functions are included
              async function a2() {}
            }
            // top-level variables are included
            let b: C
            function getB() {}
            // exported variables are included
            export const d = e;
        "#
        .unindent();

        let buffer =
            cx.new_model(|cx| language::Buffer::local(text, cx).with_language(language, cx));
        let outline = buffer.update(cx, |buffer, _| buffer.snapshot().outline(None).unwrap());
        assert_eq!(
            outline
                .items
                .iter()
                .map(|item| (item.text.as_str(), item.depth))
                .collect::<Vec<_>>(),
            &[
                ("function a()", 0),
                ("async function a2()", 1),
                ("let b", 0),
                ("function getB()", 0),
                ("const d", 0),
            ]
        );
    }
}
