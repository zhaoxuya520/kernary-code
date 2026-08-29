use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiLanguage {
    En,
    #[default]
    ZhCn,
    ZhTw,
    Ja,
}

impl UiLanguage {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::ZhCn => "zh-CN",
            Self::ZhTw => "zh-TW",
            Self::Ja => "ja",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Self::En),
            "zh-cn" | "zh_cn" | "zh-hans" | "简体中文" => Some(Self::ZhCn),
            "zh-tw" | "zh_tw" | "zh-hant" | "繁體中文" | "繁体中文" => Some(Self::ZhTw),
            "ja" | "jp" | "japanese" | "日本語" => Some(Self::Ja),
            _ => None,
        }
    }

    #[must_use]
    pub const fn pack(self) -> &'static LanguagePack {
        match self {
            Self::En => &EN,
            Self::ZhCn => &ZH_CN,
            Self::ZhTw => &ZH_TW,
            Self::Ja => &JA,
        }
    }
}

pub struct LanguagePack {
    pub activity: &'static str,
    pub input: &'static str,
    pub setup: &'static str,
    pub command_palette: &'static str,
    pub command_hint: &'static str,
    pub editor_hint: &'static str,
    pub onboarding_title: &'static str,
    pub onboarding_step_provider: &'static str,
    pub onboarding_step_commands: &'static str,
    pub model_unconfigured: &'static str,
    pub configured_language: &'static str,
    pub cancelled: &'static str,
    pub provider_add_title: &'static str,
    pub provider_url_prompt: &'static str,
    pub provider_switch_prompt: &'static str,
    pub provider_default_prompt: &'static str,
    pub vector_setup_title: &'static str,
    pub vector_setup_description: &'static str,
    pub vector_url_prompt: &'static str,
    pub vector_model_prompt: &'static str,
    pub secure_key_prompt: &'static str,
    pub select_confirm: &'static str,
    pub first_model_hint: &'static str,
    pub default_model_label: &'static str,
    pub model_ready: &'static str,
    pub connect_first: &'static str,
    pub choose_as_default: &'static str,
    pub remove_provider_hint: &'static str,
    pub session_label: &'static str,
    pub goal_label: &'static str,
    pub model_label: &'static str,
    pub reasoning_label: &'static str,
    pub limits_label: &'static str,
    pub capability_label: &'static str,
    pub mode_label: &'static str,
    pub context_label: &'static str,
    pub cache_label: &'static str,
    pub agents_label: &'static str,
    pub provider_switched: &'static str,
    pub provider_verified: &'static str,
    pub models_fetched: &'static str,
    pub vector_verified: &'static str,
    pub dimensions_label: &'static str,
    pub vector_saved_restart: &'static str,
    pub vector_key_saved: &'static str,
}

pub(crate) fn command_description(
    language: UiLanguage,
    name: &str,
    zh_cn: &'static str,
) -> &'static str {
    if language == UiLanguage::ZhCn {
        return zh_cn;
    }
    let (en, zh_tw, ja) = match name {
        "/account" => (
            "Show account metadata without secrets",
            "顯示不含密鑰的帳戶中繼資料",
            "秘密を含まないアカウント情報を表示します",
        ),
        "/agent" => (
            "Inspect one agent's role, capabilities, and lifecycle",
            "檢視單一 Agent 的角色、能力與生命週期",
            "単一エージェントの役割・能力・状態を確認します",
        ),
        "/agents" => (
            "List agents using normal, verbose, compact, or tree views",
            "以標準、詳細、精簡或樹狀模式列出 Agent",
            "エージェントを標準・詳細・簡潔・ツリー表示します",
        ),
        "/approve" => (
            "Approve a waiting tool invocation",
            "核准等待中的工具呼叫",
            "待機中のツール実行を承認します",
        ),
        "/browser" => (
            "Control the isolated browser runtime",
            "控制隔離的瀏覽器執行環境",
            "分離されたブラウザー実行環境を操作します",
        ),
        "/budget" => (
            "Inspect or change agent execution budgets",
            "檢視或調整 Agent 執行預算",
            "エージェント実行予算を確認・変更します",
        ),
        "/cache" => (
            "Show L1/L2 cache metrics",
            "顯示 L1/L2 快取指標",
            "L1/L2 キャッシュの指標を表示します",
        ),
        "/checkpoint" => (
            "Create a durable context checkpoint",
            "建立可持久化的 Context 檢查點",
            "永続コンテキストのチェックポイントを作成します",
        ),
        "/clear" => (
            "Clear the terminal viewport only",
            "只清除終端顯示內容",
            "端末の表示領域だけを消去します",
        ),
        "/compact" => (
            "Checkpoint and compact context safely",
            "建立檢查點並安全壓縮 Context",
            "チェックポイント後にコンテキストを安全に圧縮します",
        ),
        "/config" => (
            "Show effective layered configuration and provenance",
            "顯示有效分層設定與來源",
            "有効な階層設定と由来を表示します",
        ),
        "/connect" => (
            "Securely connect a built-in model provider",
            "安全連接內建模型提供商",
            "組み込みモデルプロバイダーへ安全に接続します",
        ),
        "/context" => (
            "Inspect context series, budget, filters, and checkpoints",
            "檢視 Context 系列、預算、篩選與檢查點",
            "コンテキスト系列・予算・フィルター・チェックポイントを確認します",
        ),
        "/debug" => (
            "Show a read-only runtime debug snapshot",
            "顯示唯讀執行階段除錯快照",
            "読み取り専用の実行時デバッグ情報を表示します",
        ),
        "/deny" => (
            "Deny a waiting tool invocation",
            "拒絕等待中的工具呼叫",
            "待機中のツール実行を拒否します",
        ),
        "/diff" => (
            "Show the current Git diff",
            "顯示目前 Git 差異",
            "現在の Git 差分を表示します",
        ),
        "/doctor" => (
            "Run read-only runtime diagnostics",
            "執行唯讀執行階段診斷",
            "読み取り専用の実行時診断を行います",
        ),
        "/exit" => (
            "Save state and exit",
            "儲存狀態並離開",
            "状態を保存して終了します",
        ),
        "/failover" => (
            "Manage explicit cost-confirmed model failover",
            "管理需確認成本的明確模型容錯切換",
            "費用確認付きの明示的モデル切替を管理します",
        ),
        "/focus" => (
            "Set or clear the durable retrieval focus",
            "設定或清除持久化檢索焦點",
            "永続検索フォーカスを設定・解除します",
        ),
        "/forget" => (
            "Delete one project-memory record",
            "刪除一筆專案記憶",
            "プロジェクトメモリを1件削除します",
        ),
        "/fork" => (
            "Fork a child session from a checkpoint",
            "從檢查點分支子 Session",
            "チェックポイントから子セッションを分岐します",
        ),
        "/git" => (
            "Run allowlisted read-only Git operations",
            "執行允許清單內的唯讀 Git 操作",
            "許可済みの読み取り専用 Git 操作を行います",
        ),
        "/goal" => (
            "View and manage the durable goal",
            "檢視與管理持久化 Goal",
            "永続ゴールを表示・管理します",
        ),
        "/help" => (
            "Show command help",
            "顯示命令說明",
            "コマンドのヘルプを表示します",
        ),
        "/index" => (
            "Build, update, inspect, or search the repository index",
            "建立、更新、檢視或搜尋 Repository Index",
            "リポジトリ索引を構築・更新・確認・検索します",
        ),
        "/inspect" => (
            "Inspect one harness subsystem",
            "檢視單一 Harness 子系統",
            "Harness のサブシステムを1つ確認します",
        ),
        "/language" => (
            "Switch the customized terminal language pack",
            "切換高度客製化的終端語言包",
            "専用に調整された端末言語パックを切り替えます",
        ),
        "/logout" => (
            "Remove a provider credential from the OS store",
            "從作業系統憑證庫移除 Provider 憑證",
            "OS 資格情報ストアからプロバイダー認証を削除します",
        ),
        "/logs" => (
            "Show recent redacted runtime events",
            "顯示近期已遮蔽的執行階段事件",
            "秘匿化された最近の実行イベントを表示します",
        ),
        "/lsp" => (
            "Query LSP and manage safe edit previews",
            "查詢 LSP 並管理安全編輯預覽",
            "LSP の照会と安全な編集プレビューを管理します",
        ),
        "/mcp" => (
            "Manage lazy MCP servers and their capabilities",
            "管理惰性 MCP Server 與能力",
            "遅延起動の MCP サーバーと機能を管理します",
        ),
        "/memory" => (
            "Manage structured project memory and retrieval",
            "管理結構化專案記憶與檢索",
            "構造化プロジェクトメモリと検索を管理します",
        ),
        "/mode" => (
            "Switch the real runtime resource profile",
            "切換實際執行資源模式",
            "実際の実行リソースプロファイルを切り替えます",
        ),
        "/model" => (
            "Switch the model within the current provider",
            "切換目前提供商內的模型",
            "現在のプロバイダー内でモデルを切り替えます",
        ),
        "/models" => (
            "List models or explicitly refresh one provider catalog",
            "列出模型或明確更新單一提供商目錄",
            "モデル一覧または単一プロバイダーの明示更新を行います",
        ),
        "/patch" => (
            "List durable patch records",
            "列出持久化 Patch 記錄",
            "永続パッチ記録を一覧表示します",
        ),
        "/permissions" => (
            "Manage permission modes and durable rules",
            "管理權限模式與持久規則",
            "権限モードと永続ルールを管理します",
        ),
        "/pin" => (
            "Add context that compaction cannot remove",
            "加入不會被壓縮移除的 Context",
            "圧縮で削除されないコンテキストを追加します",
        ),
        "/plan" => (
            "Show the current task plan",
            "顯示目前任務計畫",
            "現在のタスク計画を表示します",
        ),
        "/plugins" => (
            "Review and manage isolated plugins",
            "審查並管理隔離 Plugin",
            "分離プラグインを審査・管理します",
        ),
        "/profile" => (
            "Show measured runtime latency profiles",
            "顯示實測執行延遲分析",
            "実測された実行レイテンシを表示します",
        ),
        "/provider" => (
            "Show, add, or switch text-model providers",
            "顯示、新增或切換文字模型提供商",
            "テキストモデルのプロバイダーを表示・追加・切替します",
        ),
        "/providers" => (
            "List provider routes and credential readiness",
            "列出提供商路由與憑證狀態",
            "プロバイダーの経路と認証状態を一覧表示します",
        ),
        "/queue" => (
            "Inspect, cancel, or reprioritize queued work",
            "檢視、取消或調整排程工作優先度",
            "待機作業を確認・取消・優先度変更します",
        ),
        "/reasoning" => (
            "Set model reasoning effort with visible clamping",
            "設定模型推理強度並顯示能力限制",
            "モデルの推論強度を設定し制限結果を表示します",
        ),
        "/reset" => (
            "Checkpoint then reset nonessential context",
            "建立檢查點後重設非必要 Context",
            "チェックポイント後に不要なコンテキストをリセットします",
        ),
        "/resume" => (
            "Resume a recoverable agent team",
            "恢復可復原的 Agent Team",
            "復旧可能なエージェントチームを再開します",
        ),
        "/retry" => (
            "Retry a safe failed tool invocation",
            "重試可安全重複的失敗工具",
            "安全に再試行できる失敗ツールを再実行します",
        ),
        "/review" => (
            "Run a controlled code review",
            "執行受控程式碼審查",
            "制御されたコードレビューを実行します",
        ),
        "/rollback" => (
            "Restore context from a checkpoint into a new series",
            "從檢查點還原為新的 Context 系列",
            "チェックポイントから新しいコンテキスト系列へ復元します",
        ),
        "/sandbox" => (
            "Show real sandbox capabilities and hard boundaries",
            "顯示真實 Sandbox 能力與硬邊界",
            "実際のサンドボックス機能と強制境界を表示します",
        ),
        "/session" => (
            "Show the current durable session",
            "顯示目前持久化 Session",
            "現在の永続セッションを表示します",
        ),
        "/sessions" => (
            "List project sessions and fork lineage",
            "列出專案 Session 與分支關係",
            "プロジェクトのセッションと分岐関係を一覧表示します",
        ),
        "/settings" => (
            "Inspect or change session/runtime settings",
            "檢視或調整 Session/Runtime 設定",
            "セッション／実行時設定を確認・変更します",
        ),
        "/skills" => (
            "Search and lazily load skills",
            "搜尋並惰性載入 Skill",
            "スキルを検索して遅延読み込みします",
        ),
        "/status" => (
            "Show model, mode, goal, and session status",
            "顯示模型、模式、Goal 與 Session 狀態",
            "モデル・モード・ゴール・セッション状態を表示します",
        ),
        "/steer" => (
            "Send guidance to an active supervisor",
            "向執行中的 Supervisor 傳送指引",
            "実行中のスーパーバイザーへ指示を送ります",
        ),
        "/team" => (
            "Inspect or start a multi-agent team",
            "檢視或啟動多 Agent 團隊",
            "マルチエージェントチームを確認・開始します",
        ),
        "/test" => (
            "Run the configured test executable",
            "執行已設定的測試程式",
            "設定済みのテスト実行ファイルを起動します",
        ),
        "/think" => (
            "Alias for /reasoning",
            "/reasoning 的別名",
            "/reasoning の別名です",
        ),
        "/tools" => (
            "List registered tools and effect classes",
            "列出已註冊工具與副作用類型",
            "登録済みツールと効果区分を一覧表示します",
        ),
        "/trace" => (
            "Control bounded runtime tracing",
            "控制有界執行階段追蹤",
            "上限付き実行トレースを制御します",
        ),
        "/undo" => (
            "Safely undo a verified harness patch",
            "安全復原已驗證的 Harness Patch",
            "検証済み Harness パッチを安全に取り消します",
        ),
        "/vector" => (
            "Configure and validate the single embedding provider",
            "設定並驗證唯一的向量模型提供商",
            "単一の埋め込みプロバイダーを設定・検証します",
        ),
        "/why" => (
            "Show auditable decision evidence without private reasoning",
            "顯示可稽核決策證據且不洩漏私有推理",
            "非公開推論を出さず監査可能な判断根拠を表示します",
        ),
        _ => (zh_cn, zh_cn, zh_cn),
    };
    match language {
        UiLanguage::En => en,
        UiLanguage::ZhTw => zh_tw,
        UiLanguage::Ja => ja,
        UiLanguage::ZhCn => zh_cn,
    }
}

const EN: LanguagePack = LanguagePack {
    activity: "Activity",
    input: "Input",
    setup: "Setup",
    command_palette: "Commands",
    command_hint: "↑↓ select · Tab complete · Esc close",
    editor_hint: "←→ move · Home/End · ↑↓ history/commands",
    onboarding_title: "No model is configured. Test models never handle user work.",
    onboarding_step_provider: "Use /provider add for a custom endpoint, or /connect for a built-in provider.",
    onboarding_step_commands: "Type / to browse every command. Use /provider switch and /model to switch routes.",
    model_unconfigured: "Not configured",
    configured_language: "Interface language",
    cancelled: "Setup cancelled",
    provider_add_title: "Custom provider · OpenAI-compatible discovery wizard",
    provider_url_prompt: "API base URL",
    provider_switch_prompt: "Choose a model provider",
    provider_default_prompt: "Choose the default model",
    vector_setup_title: "Vector setup · one embedding provider per project",
    vector_setup_description: "Enter the model name manually. Kernary validates one real embedding before saving.",
    vector_url_prompt: "Embedding API base URL",
    vector_model_prompt: "Embedding model name (manual)",
    secure_key_prompt: "API key",
    select_confirm: "↑↓ select · Tab/Enter confirm",
    first_model_hint: "Uses the first catalog model",
    default_model_label: "Default model",
    model_ready: "ready",
    connect_first: "run /connect first",
    choose_as_default: "Set as this provider's default model",
    remove_provider_hint: "Remove project config and credential reference",
    session_label: "Session",
    goal_label: "Goal",
    model_label: "Model",
    reasoning_label: "Reasoning",
    limits_label: "Limits",
    capability_label: "Capabilities",
    mode_label: "Mode",
    context_label: "Context",
    cache_label: "Cache",
    agents_label: "Agents",
    provider_switched: "Provider switched",
    provider_verified: "Provider verified and saved",
    models_fetched: "models fetched; choose a default",
    vector_verified: "Vector provider verified",
    dimensions_label: "Dimensions",
    vector_saved_restart: "Saved; Ready after restart and activated lazily on first semantic request",
    vector_key_saved: "Key saved in the OS credential store; vector models are not listed automatically",
};

const ZH_CN: LanguagePack = LanguagePack {
    activity: "活动",
    input: "输入",
    setup: "设置向导",
    command_palette: "命令",
    command_hint: "↑↓ 选择 · Tab 补全 · Esc 关闭",
    editor_hint: "←→ 移动 · Home/End · ↑↓ 历史/命令",
    onboarding_title: "尚未配置模型；测试模型绝不会处理用户任务。",
    onboarding_step_provider: "自定义中转站使用 /provider add；内置提供商使用 /connect。",
    onboarding_step_commands: "输入 / 浏览全部命令；/provider switch 切换提供商，/model 切换当前模型。",
    model_unconfigured: "未配置",
    configured_language: "界面语言",
    cancelled: "设置已取消",
    provider_add_title: "自定义提供商 · OpenAI-compatible 自动发现向导",
    provider_url_prompt: "API Base URL",
    provider_switch_prompt: "选择模型提供商",
    provider_default_prompt: "选择默认模型",
    vector_setup_title: "向量设置 · 每个项目一个 Embedding Provider",
    vector_setup_description: "模型名由你手动输入；保存前执行一次真实 Embedding 验证。",
    vector_url_prompt: "Embedding API Base URL",
    vector_model_prompt: "手动输入 Embedding 模型名",
    secure_key_prompt: "API Key",
    select_confirm: "↑↓ 选择 · Tab/Enter 确认",
    first_model_hint: "选择后使用目录中的首个模型",
    default_model_label: "默认模型",
    model_ready: "已连接",
    connect_first: "先运行 /connect",
    choose_as_default: "设为该 Provider 的默认模型",
    remove_provider_hint: "删除项目配置与凭证引用",
    session_label: "会话",
    goal_label: "目标",
    model_label: "模型",
    reasoning_label: "推理",
    limits_label: "限制",
    capability_label: "能力",
    mode_label: "模式",
    context_label: "上下文",
    cache_label: "缓存",
    agents_label: "智能体",
    provider_switched: "已切换提供商",
    provider_verified: "提供商已验证并保存",
    models_fetched: "个模型已获取；请选择默认模型",
    vector_verified: "向量提供商已验证",
    dimensions_label: "维度",
    vector_saved_restart: "已保存；重启后进入 Ready，首次语义请求再惰性激活",
    vector_key_saved: "Key 已进入系统凭证库；不会自动拉取向量模型目录",
};

const ZH_TW: LanguagePack = LanguagePack {
    activity: "活動",
    input: "輸入",
    setup: "設定精靈",
    command_palette: "命令",
    command_hint: "↑↓ 選擇 · Tab 補全 · Esc 關閉",
    editor_hint: "←→ 移動 · Home/End · ↑↓ 歷史/命令",
    onboarding_title: "尚未設定模型；測試模型絕不會處理使用者任務。",
    onboarding_step_provider: "自訂中轉站使用 /provider add；內建提供商使用 /connect。",
    onboarding_step_commands: "輸入 / 瀏覽全部命令；/provider switch 切換提供商，/model 切換目前模型。",
    model_unconfigured: "未設定",
    configured_language: "介面語言",
    cancelled: "設定已取消",
    provider_add_title: "自訂提供商 · OpenAI-compatible 自動探索精靈",
    provider_url_prompt: "API Base URL",
    provider_switch_prompt: "選擇模型提供商",
    provider_default_prompt: "選擇預設模型",
    vector_setup_title: "向量設定 · 每個專案一個 Embedding Provider",
    vector_setup_description: "模型名稱由你手動輸入；儲存前執行一次真實 Embedding 驗證。",
    vector_url_prompt: "Embedding API Base URL",
    vector_model_prompt: "手動輸入 Embedding 模型名稱",
    secure_key_prompt: "API Key",
    select_confirm: "↑↓ 選擇 · Tab/Enter 確認",
    first_model_hint: "選擇後使用目錄中的第一個模型",
    default_model_label: "預設模型",
    model_ready: "已連接",
    connect_first: "請先執行 /connect",
    choose_as_default: "設為此提供商的預設模型",
    remove_provider_hint: "刪除專案設定與憑證參照",
    session_label: "工作階段",
    goal_label: "目標",
    model_label: "模型",
    reasoning_label: "推理",
    limits_label: "限制",
    capability_label: "能力",
    mode_label: "模式",
    context_label: "上下文",
    cache_label: "快取",
    agents_label: "代理",
    provider_switched: "已切換提供商",
    provider_verified: "提供商已驗證並儲存",
    models_fetched: "個模型已取得；請選擇預設模型",
    vector_verified: "向量提供商已驗證",
    dimensions_label: "維度",
    vector_saved_restart: "已儲存；重新啟動後進入 Ready，首次語意請求時才延遲啟用",
    vector_key_saved: "Key 已進入系統憑證庫；不會自動載入向量模型目錄",
};

const JA: LanguagePack = LanguagePack {
    activity: "アクティビティ",
    input: "入力",
    setup: "セットアップ",
    command_palette: "コマンド",
    command_hint: "↑↓ 選択 · Tab 補完 · Esc 閉じる",
    editor_hint: "←→ 移動 · Home/End · ↑↓ 履歴/コマンド",
    onboarding_title: "モデルが未設定です。テストモデルはユーザーの作業を実行しません。",
    onboarding_step_provider: "独自エンドポイントは /provider add、組み込みプロバイダーは /connect を使います。",
    onboarding_step_commands: "/ で全コマンドを表示し、/provider switch と /model で切り替えます。",
    model_unconfigured: "未設定",
    configured_language: "表示言語",
    cancelled: "セットアップを中止しました",
    provider_add_title: "カスタムプロバイダー · OpenAI 互換の自動検出ウィザード",
    provider_url_prompt: "API ベース URL",
    provider_switch_prompt: "モデルプロバイダーを選択",
    provider_default_prompt: "既定モデルを選択",
    vector_setup_title: "ベクトル設定 · プロジェクトごとに1つの埋め込みプロバイダー",
    vector_setup_description: "モデル名は手動入力し、保存前に実際の埋め込み応答を検証します。",
    vector_url_prompt: "埋め込み API ベース URL",
    vector_model_prompt: "埋め込みモデル名を手動入力",
    secure_key_prompt: "API キー",
    select_confirm: "↑↓ 選択 · Tab/Enter 確定",
    first_model_hint: "カタログの先頭モデルを使用します",
    default_model_label: "既定モデル",
    model_ready: "接続済み",
    connect_first: "先に /connect を実行してください",
    choose_as_default: "このプロバイダーの既定モデルに設定します",
    remove_provider_hint: "プロジェクト設定と認証参照を削除します",
    session_label: "セッション",
    goal_label: "ゴール",
    model_label: "モデル",
    reasoning_label: "推論",
    limits_label: "上限",
    capability_label: "機能",
    mode_label: "モード",
    context_label: "コンテキスト",
    cache_label: "キャッシュ",
    agents_label: "エージェント",
    provider_switched: "プロバイダーを切り替えました",
    provider_verified: "プロバイダーを検証して保存しました",
    models_fetched: "件のモデルを取得しました。既定モデルを選択してください",
    vector_verified: "ベクトルプロバイダーを検証しました",
    dimensions_label: "次元数",
    vector_saved_restart: "保存しました。再起動後に Ready となり、最初の意味検索で遅延起動します",
    vector_key_saved: "キーを OS 資格情報ストアへ保存しました。ベクトルモデル一覧は自動取得しません",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_and_packs_are_distinct() {
        for (code, language) in [
            ("en", UiLanguage::En),
            ("zh-CN", UiLanguage::ZhCn),
            ("zh-TW", UiLanguage::ZhTw),
            ("ja", UiLanguage::Ja),
        ] {
            assert_eq!(UiLanguage::parse(code), Some(language));
            assert_eq!(language.code(), code);
            assert!(!language.pack().onboarding_title.is_empty());
        }
        assert_ne!(
            UiLanguage::En.pack().activity,
            UiLanguage::Ja.pack().activity
        );
    }
}
