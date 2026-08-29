import type { ToolDescriptor, ToolProvider, ToolRegistry } from "../../tool-runtime/src/tool-runtime.ts";

export type PluginLifecycleStatus =
  | "installed"
  | "disabled"
  | "activating"
  | "active"
  | "draining"
  | "failed";

export interface PluginManifest {
  id: string;
  name: string;
  version: string;
  description: string;
  engineRange: string;
  permissions: string[];
  contributions: {
    skills?: string[];
    tools?: string[];
    mcpServers?: string[];
    contextProviders?: string[];
    settingsSchema?: string;
    uiPanels?: string[];
  };
}

export interface ServiceDefinition<T> {
  id: string;
  version: string;
  cardinality: "one" | "many";
  coreOwned?: boolean;
  validate(value: unknown): T;
}

interface ServiceProviderRecord {
  pluginId: string;
  scope: string;
  value: unknown;
}

/** DeepSeek Harness 风格的 Definition/Provider/Consumer，但核心服务不可被插件替换。 */
export class ServiceRegistry {
  readonly #definitions = new Map<string, ServiceDefinition<unknown>>();
  readonly #providers = new Map<string, ServiceProviderRecord[]>();

  define<T>(definition: ServiceDefinition<T>): ServiceDefinition<T> {
    const existing = this.#definitions.get(definition.id);
    if (existing && existing.version !== definition.version) {
      throw new Error(`service-definition-version-conflict: ${definition.id}`);
    }
    this.#definitions.set(definition.id, definition as ServiceDefinition<unknown>);
    return definition;
  }

  provide<T>(input: {
    pluginId: string;
    scope: string;
    definition: ServiceDefinition<T>;
    value: unknown;
  }): () => void {
    const registered = this.#definitions.get(input.definition.id);
    if (!registered) throw new Error(`service-definition-not-registered: ${input.definition.id}`);
    if (registered.coreOwned) throw new Error(`core-service-cannot-be-provided-by-plugin: ${registered.id}`);
    const value = input.definition.validate(input.value);
    const providers = this.#providers.get(registered.id) ?? [];
    if (
      registered.cardinality === "one" &&
      providers.some((provider) => provider.scope === input.scope)
    ) {
      throw new Error(`single-service-provider-conflict: ${registered.id}@${input.scope}`);
    }
    const record: ServiceProviderRecord = {
      pluginId: input.pluginId,
      scope: input.scope,
      value,
    };
    providers.push(record);
    this.#providers.set(registered.id, providers);
    return () => {
      const current = this.#providers.get(registered.id) ?? [];
      this.#providers.set(
        registered.id,
        current.filter((provider) => provider !== record),
      );
    };
  }

  consumeOne<T>(definition: ServiceDefinition<T>, scope: string): T | undefined {
    const provider = (this.#providers.get(definition.id) ?? []).find((candidate) => candidate.scope === scope);
    return provider?.value as T | undefined;
  }

  consumeMany<T>(definition: ServiceDefinition<T>, scope: string): T[] {
    return (this.#providers.get(definition.id) ?? [])
      .filter((provider) => provider.scope === scope)
      .map((provider) => provider.value as T);
  }

  countProviders(pluginId?: string): number {
    return [...this.#providers.values()]
      .flat()
      .filter((provider) => !pluginId || provider.pluginId === pluginId).length;
  }
}

export interface PluginActivationContext {
  readonly pluginId: string;
  readonly scope: string;
  provideService<T>(definition: ServiceDefinition<T>, value: unknown): void;
  registerTool<Args, Result>(descriptor: ToolDescriptor<Args, Result>, provider: ToolProvider<Args, Result>): void;
  onDispose(disposer: () => void | Promise<void>): void;
}

export interface PluginModule {
  activate(context: PluginActivationContext): void | (() => void | Promise<void>) | Promise<void | (() => void | Promise<void>)>;
}

interface InstalledPlugin {
  manifest: PluginManifest;
  module: PluginModule;
  status: PluginLifecycleStatus;
  scope?: string;
  disposers: Array<() => void | Promise<void>>;
  lastError?: string;
}

export interface PluginView {
  id: string;
  name: string;
  version: string;
  description: string;
  status: PluginLifecycleStatus;
  scope?: string;
  permissions: string[];
  contributions: PluginManifest["contributions"];
  activeContributionCount: number;
  lastError?: string;
}

function nonEmptyString(value: unknown, field: string): string {
  if (typeof value !== "string" || !value.trim()) throw new Error(`invalid-plugin-manifest-field: ${field}`);
  return value;
}

export function validatePluginManifest(value: unknown): PluginManifest {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("invalid-plugin-manifest");
  const input = value as Record<string, unknown>;
  const permissions = input.permissions;
  const contributions = input.contributions;
  if (!Array.isArray(permissions) || permissions.some((item) => typeof item !== "string")) {
    throw new Error("invalid-plugin-permissions");
  }
  if (typeof contributions !== "object" || contributions === null || Array.isArray(contributions)) {
    throw new Error("invalid-plugin-contributions");
  }
  const contributionInput = contributions as Record<string, unknown>;
  const stringArray = (field: string): string[] | undefined => {
    const fieldValue = contributionInput[field];
    if (fieldValue === undefined) return undefined;
    if (!Array.isArray(fieldValue) || fieldValue.some((item) => typeof item !== "string")) {
      throw new Error(`invalid-plugin-contribution: ${field}`);
    }
    return fieldValue as string[];
  };
  const skills = stringArray("skills");
  const tools = stringArray("tools");
  const mcpServers = stringArray("mcpServers");
  const contextProviders = stringArray("contextProviders");
  const uiPanels = stringArray("uiPanels");
  const settingsSchema = contributionInput.settingsSchema;
  if (settingsSchema !== undefined && typeof settingsSchema !== "string") {
    throw new Error("invalid-plugin-contribution: settingsSchema");
  }
  return {
    id: nonEmptyString(input.id, "id"),
    name: nonEmptyString(input.name, "name"),
    version: nonEmptyString(input.version, "version"),
    description: nonEmptyString(input.description, "description"),
    engineRange: nonEmptyString(input.engineRange, "engineRange"),
    permissions: [...permissions] as string[],
    contributions: {
      ...(skills ? { skills } : {}),
      ...(tools ? { tools } : {}),
      ...(mcpServers ? { mcpServers } : {}),
      ...(contextProviders ? { contextProviders } : {}),
      ...(settingsSchema ? { settingsSchema } : {}),
      ...(uiPanels ? { uiPanels } : {}),
    },
  };
}

export class PluginHost {
  readonly services: ServiceRegistry;
  readonly tools: ToolRegistry;
  readonly #plugins = new Map<string, InstalledPlugin>();

  constructor(input: { services: ServiceRegistry; tools: ToolRegistry }) {
    this.services = input.services;
    this.tools = input.tools;
  }

  install(manifestInput: unknown, module: PluginModule): PluginView {
    const manifest = validatePluginManifest(manifestInput);
    if (this.#plugins.has(manifest.id)) throw new Error(`plugin-already-installed: ${manifest.id}`);
    const plugin: InstalledPlugin = {
      manifest,
      module,
      status: "installed",
      disposers: [],
    };
    this.#plugins.set(manifest.id, plugin);
    plugin.status = "disabled";
    return this.view(plugin);
  }

  async enable(pluginId: string, scope: string): Promise<PluginView> {
    const plugin = this.plugin(pluginId);
    if (plugin.status === "active") return this.view(plugin);
    if (plugin.status === "activating" || plugin.status === "draining") {
      throw new Error(`plugin-busy: ${pluginId}`);
    }
    plugin.status = "activating";
    plugin.scope = scope;
    plugin.lastError = undefined;
    const disposers: InstalledPlugin["disposers"] = [];
    const context: PluginActivationContext = {
      pluginId,
      scope,
      provideService: (definition, value) => {
        disposers.push(this.services.provide({ pluginId, scope, definition, value }));
      },
      registerTool: (descriptor, provider) => {
        disposers.push(this.tools.register(descriptor, provider));
      },
      onDispose: (disposer) => disposers.push(disposer),
    };

    try {
      const moduleDisposer = await plugin.module.activate(context);
      if (moduleDisposer) disposers.push(moduleDisposer);
      plugin.disposers = disposers;
      plugin.status = "active";
    } catch (error) {
      for (const dispose of disposers.reverse()) {
        try {
          await dispose();
        } catch {
          // 回滚尽最大努力；原始激活错误保留为主要诊断。
        }
      }
      plugin.disposers = [];
      plugin.status = "failed";
      plugin.lastError = error instanceof Error ? error.message : String(error);
    }
    return this.view(plugin);
  }

  async disable(pluginId: string): Promise<PluginView> {
    const plugin = this.plugin(pluginId);
    if (plugin.status === "disabled") return this.view(plugin);
    plugin.status = "draining";
    for (const dispose of plugin.disposers.splice(0).reverse()) await dispose();
    plugin.status = "disabled";
    plugin.scope = undefined;
    return this.view(plugin);
  }

  list(): PluginView[] {
    return [...this.#plugins.values()].map((plugin) => this.view(plugin)).sort((a, b) => a.name.localeCompare(b.name));
  }

  private plugin(pluginId: string): InstalledPlugin {
    const plugin = this.#plugins.get(pluginId);
    if (!plugin) throw new Error(`plugin-not-installed: ${pluginId}`);
    return plugin;
  }

  private view(plugin: InstalledPlugin): PluginView {
    return {
      id: plugin.manifest.id,
      name: plugin.manifest.name,
      version: plugin.manifest.version,
      description: plugin.manifest.description,
      status: plugin.status,
      ...(plugin.scope ? { scope: plugin.scope } : {}),
      permissions: [...plugin.manifest.permissions],
      contributions: plugin.manifest.contributions,
      activeContributionCount: plugin.disposers.length,
      ...(plugin.lastError ? { lastError: plugin.lastError } : {}),
    };
  }
}

/** Profile/Bundle/Override 使用递归对象合并；数组和标量由后层整体替换。 */
export function composePluginSettings(...layers: JsonValue[]): JsonValue {
  const merge = (left: JsonValue, right: JsonValue): JsonValue => {
    if (
      typeof left === "object" &&
      left !== null &&
      !Array.isArray(left) &&
      typeof right === "object" &&
      right !== null &&
      !Array.isArray(right)
    ) {
      const result: Record<string, JsonValue> = { ...(left as Record<string, JsonValue>) };
      for (const [key, value] of Object.entries(right)) result[key] = key in result ? merge(result[key] ?? null, value) : value;
      return result;
    }
    return right;
  };
  return layers.reduce<JsonValue>((current, layer) => merge(current, layer), {});
}

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
