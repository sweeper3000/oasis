<script lang="ts">
  import { onMount, tick } from "svelte";
  import { t } from "svelte-i18n";
  import type { IStorageConfig, IStorageProvider, StorageProtocol } from "../utils/types";

  export let onSaved: (config: IStorageConfig) => void = () => {};
  let config: IStorageConfig = { enabled: false, mode: "mount", main_provider: "", providers: [] };
  let form: HTMLFormElement;
  let loading = true;
  let busy = false;
  let error = "";
  let message = "";
  let activeProvider = "";
  let choosingProtocol = false;
  let newProtocol: StorageProtocol | "" = "";
  let savedProviders: Record<string, string> = {};
  const protocols: StorageProtocol[] = ["s3", "sftp", "ftps", "ftp", "webdav", "smb"];
  const label = (key: string) => $t(`component.external_storage.${key}`);

  async function request(method: string, suffix = "", body?: IStorageConfig) {
    const response = await fetch(`/api/sys/storage${suffix}`, {
      method,
      headers: { "Content-Type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!response.ok) {
      const details = await response.json().catch(() => null);
      throw new Error(details && details.error ? details.error : label("request_failed"));
    }
    return response;
  }

  onMount(async () => {
    try {
      config = await (await request("GET")).json();
      rememberSaved();
      onSaved(config);
    } catch (e) { error = e.message; }
    finally { loading = false; }
  });

  const rememberSaved = () => {
    savedProviders = Object.fromEntries(config.providers.map(provider => [provider.id, JSON.stringify(provider)]));
  };
  const isUnsaved = (provider: IStorageProvider) => savedProviders[provider.id] !== JSON.stringify(provider);
  const startAdding = () => {
    activeProvider = "";
    newProtocol = "";
    choosingProtocol = true;
    message = "";
  };
  const editProvider = (id: string) => {
    choosingProtocol = false;
    activeProvider = id;
    message = "";
  };
  const addProvider = async () => {
    if (!newProtocol || config.providers.length >= 16) return;
    const id = `drive-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;
    const provider: IStorageProvider = {
      id, name: "", protocol: newProtocol, root: "", host: "", port: 0, username: "", password: "",
      endpoint: "", region: "us-east-1", access_key_id: "", secret_access_key: "", session_token: "",
      key_file: "", known_hosts_file: "", domain: "", implicit_tls: false, encryption: false, encryption_key: "",
    };
    config.providers = [...config.providers, provider];
    if (!config.main_provider) config.main_provider = id;
    editProvider(id);
    await tick();
    form.querySelector<HTMLInputElement>(`[data-provider-id="${id}"] input`)?.focus();
  };

  const removeProvider = (id: string) => {
    config.providers = config.providers.filter(p => p.id !== id);
    if (config.main_provider === id) config.main_provider = config.providers.length ? config.providers[0].id : "";
    if (activeProvider === id) activeProvider = "";
    message = "";
  };

  type FieldKey = "host" | "username" | "password" | "endpoint" | "region" | "access_key_id" | "secret_access_key" | "session_token" | "key_file" | "known_hosts_file" | "domain";
  type Field = { key: FieldKey; secret?: boolean; required?: boolean };
  const fields = (provider: IStorageProvider): Field[] => {
    if (provider.protocol === "s3") return [
      { key: "endpoint" }, { key: "region" }, { key: "access_key_id" },
      { key: "secret_access_key", secret: true }, { key: "session_token", secret: true },
    ];
    const result: Field[] = [
      provider.protocol === "webdav" ? { key: "endpoint", required: true } : { key: "host", required: true },
      { key: "username" }, { key: "password", secret: true },
    ];
    if (provider.protocol === "sftp") result.push({ key: "key_file" }, { key: "known_hosts_file", required: true });
    if (provider.protocol === "smb") result.push({ key: "domain" });
    return result;
  };
  const setField = (provider: IStorageProvider, key: FieldKey, event: Event) => {
    provider[key] = (event.target as HTMLInputElement).value;
    config = config;
  };
  const hasSecret = (provider: IStorageProvider, key: string) => !!provider[`${key}_set`];

  const save = async (test: boolean) => {
    // Inactive editors stay mounted to preserve drafts and browser constraints.
    // Reveal the first invalid editor before asking the browser to focus it.
    const invalid = Array.from(form.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input, select"))
      .find(input => input.willValidate && !input.validity.valid);
    if (invalid) {
      const editor = invalid.closest<HTMLElement>("[data-provider-id]");
      if (editor) editProvider(editor.dataset.providerId);
      await tick();
      invalid.reportValidity();
      return;
    }
    busy = true; error = ""; message = "";
    try {
      const response = await request(test ? "POST" : "PUT", test ? "/test" : "", config);
      if (test) message = label("test_success");
      else {
        config = await response.json();
        rememberSaved();
        activeProvider = "";
        choosingProtocol = false;
        onSaved(config);
        message = label("saved");
      }
    } catch (e) { error = e.message; }
    finally { busy = false; }
  };
</script>

<section class="bg-gray-50 shadow rounded-lg p-4 md:p-8 mt-6" aria-labelledby="external-storage-title">
  <h2 id="external-storage-title" class="text-xl font-bold text-gray-700 mb-3">{label("title")}</h2>
  <p class="text-sm mb-4">{label("intro")}</p>
  {#if loading}
    <p role="status">{label("loading")}</p>
  {:else}
    <form bind:this={form} novalidate on:submit|preventDefault={() => save(false)}>
      <fieldset disabled={busy}>
        <label class="flex items-center mb-4">
          <input type="checkbox" bind:checked={config.enabled} class="mr-2" />
          {label("enable")}
        </label>
        {#if config.enabled}
          <label class="block mb-2" for="storage-mode">{label("mode")}</label>
          <select id="storage-mode" bind:value={config.mode} class="w-full border rounded p-2 mb-2">
            <option value="main">{label("main")}</option>
            <option value="mount">{label("mount")}</option>
          </select>
          <p class="text-sm mb-4">{label(config.mode === "main" ? "main_help" : "mount_help")}</p>
          {#if config.mode === "main"}
            <label for="main-provider" class="block mb-2">{label("main_provider")}</label>
            <select id="main-provider" required bind:value={config.main_provider} class="w-full border rounded p-2 mb-4">
              <option value="" disabled>{label("select_provider")}</option>
              {#each config.providers as provider}<option value={provider.id}>{provider.name || label("unnamed")}</option>{/each}
            </select>
          {/if}
          <h3 class="font-bold text-gray-700 mb-2">{label("storage_list")}</h3>
          <p class="text-sm mb-3">{label("list_help")}</p>
          {#if !config.providers.length}
            <p class="text-sm text-gray-600 border rounded p-4 mb-4">{label("empty_list")}</p>
          {:else}
            <ul class="mb-4 divide-y border rounded" aria-label={label("storage_list")}>
              {#each config.providers as provider (provider.id)}
                <li class="flex flex-wrap items-center justify-between gap-3 p-3">
                  <div class="min-w-0">
                    <p class="font-bold break-words">{provider.name || label("unnamed")}</p>
                    <p class="text-sm text-gray-600">
                      {provider.protocol.toUpperCase()} · {label(provider.encryption ? "encrypted" : "unencrypted")}
                      {#if isUnsaved(provider)}<span class="text-yellow-800"> · {label("unsaved")}</span>{/if}
                    </p>
                  </div>
                  <div class="flex gap-3">
                    <button type="button" class="text-blue-700 underline" aria-expanded={activeProvider === provider.id}
                      aria-controls={`storage-editor-${provider.id}`} on:click={() => editProvider(provider.id)}>{label("edit")}</button>
                    <button type="button" class="text-red-700 underline" on:click={() => removeProvider(provider.id)}>{label("remove")}</button>
                  </div>
                </li>
              {/each}
            </ul>
          {/if}
          <button type="button" disabled={config.providers.length >= 16 || choosingProtocol} class="border rounded px-3 py-2 mb-4" on:click={startAdding}>{label("add")}</button>
          {#if choosingProtocol}
            <div class="border rounded p-4 mb-4">
              <label for="new-storage-protocol" class="block mb-2">{label("protocol")}</label>
              <select id="new-storage-protocol" bind:value={newProtocol} class="w-full border rounded p-2 mb-3">
                <option value="" disabled>{label("select_protocol")}</option>
                {#each protocols as protocol}<option value={protocol}>{protocol.toUpperCase()}</option>{/each}
              </select>
              <div class="flex gap-3">
                <button type="button" disabled={!newProtocol} class="border rounded px-3 py-2 disabled:opacity-50" on:click={addProvider}>{label("continue")}</button>
                <button type="button" class="underline" on:click={() => choosingProtocol = false}>{label("cancel")}</button>
              </div>
            </div>
          {/if}
          {#each config.providers as provider, index (provider.id)}
            <fieldset id={`storage-editor-${provider.id}`} data-provider-id={provider.id} class="border rounded p-4 mb-4" class:hidden={activeProvider !== provider.id}>
              <legend class="px-2 font-bold">{provider.name || `${label("provider")} ${index + 1}`}</legend>
              <div class="grid gap-3 md:grid-cols-2">
                <label class="block">{label("name")}
                  <input required maxlength="100" bind:value={provider.name} class="block w-full border rounded p-2" />
                </label>
                <label class="block">{label("protocol")}
                  <select bind:value={provider.protocol} class="block w-full border rounded p-2">
                    {#each protocols as protocol}<option value={protocol}>{protocol.toUpperCase()}</option>{/each}
                  </select>
                </label>
                <label class="block md:col-span-2">{label(provider.protocol === "s3" ? "bucket_root" : "root")}
                  <input required={provider.protocol === "s3" || provider.protocol === "smb"} bind:value={provider.root} class="block w-full border rounded p-2" />
                </label>
                {#each fields(provider) as field}
                  <label class="block">{label(field.key)}
                    <input type={field.secret ? "password" : "text"} required={field.required || false}
                      autocomplete={field.secret ? "new-password" : "off"}
                      placeholder={field.secret && hasSecret(provider, field.key) ? label("stored_secret") : ""}
                      value={provider[field.key]} on:input={(event) => setField(provider, field.key, event)} class="block w-full border rounded p-2" />
                  </label>
                {/each}
                {#if provider.protocol !== "s3" && provider.protocol !== "webdav"}
                  <label class="block">{label("port")}
                    <input type="number" min="0" max="65535" bind:value={provider.port} class="block w-full border rounded p-2" />
                  </label>
                {/if}
              </div>
              {#if provider.protocol === "s3"}<p class="text-sm mt-2">{label("s3_help")}</p>{/if}
              {#if provider.protocol === "sftp"}<p class="text-sm mt-2">{label("sftp_help")}</p>{/if}
              {#if provider.protocol === "ftps"}
                <label class="block mt-3"><input type="checkbox" bind:checked={provider.implicit_tls} /> {label("implicit_tls")}</label>
              {/if}
              {#if provider.protocol === "ftp"}
                <p role="alert" class="text-yellow-800 bg-yellow-100 rounded p-3 mt-3">{label("ftp_warning")}</p>
              {/if}
              {#if (provider.protocol === "s3" || provider.protocol === "webdav") && provider.endpoint.startsWith("http://")}
                <p role="alert" class="text-yellow-800 bg-yellow-100 rounded p-3 mt-3">{label("http_warning")}</p>
              {/if}
              <label class="block mt-4"><input type="checkbox" bind:checked={provider.encryption} /> {label("encryption")}</label>
              {#if provider.encryption}
                <label class="block mt-3">{label("encryption_key")}
                  <input type="password" autocomplete="new-password" minlength="12" maxlength="1024"
                    placeholder={provider.encryption_key_set ? label("stored_secret") : label("key_placeholder")}
                    bind:value={provider.encryption_key} class="block w-full border rounded p-2" />
                </label>
                <p class="text-sm mt-2">{label("key_help")}</p>
                <p role="alert" class="text-yellow-800 bg-yellow-100 rounded p-3 mt-2">{label("key_warning")}</p>
              {/if}
              <button type="button" class="mt-4 underline" on:click={() => activeProvider = ""}>{label("close_editor")}</button>
            </fieldset>
          {/each}
          <p class="text-sm mb-4">{label("temporary_help")}</p>
        {/if}
        <div class="flex flex-wrap gap-3">
          {#if config.enabled}
            <button type="button" class="border rounded px-3 py-2" disabled={!config.providers.length} on:click={() => save(true)}>{label("test")}</button>
          {/if}
          <button type="submit" class="bg-blue-400 text-white rounded px-3 py-2">{busy ? label("working") : label("save")}</button>
        </div>
      </fieldset>
    </form>
  {/if}
  {#if error}<p role="alert" class="text-red-700 mt-4 break-words">{error}</p>{/if}
  {#if message}<p role="status" class="text-green-700 mt-4">{message}</p>{/if}
</section>
