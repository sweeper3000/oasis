<script lang="ts">
  import { onDestroy } from "svelte";
  import { t } from "svelte-i18n";
  import DirBrowser from "./DirBrowser.svelte";

  type Step = "idle" | "export" | "phrase" | "challenge" | "import" | "preview";
  type Preview = { id: string; site_name: string; accounts: number; providers: number; visibility_rules: number; source_storage: string; local_storage: string };
  let step: Step = "idle";
  let job = "";
  let password = "";
  let phrase = "";
  let positions: number[] = [];
  let answers = ["", "", ""];
  let files: FileList;
  let preview: Preview;
  let localStorage = "";
  let confirmed = false;
  let busy = false;
  let restoring = false;
  let error = "";
  let message = "";
  let manualCopy = false;
  let browse = false;
  let controller: AbortController;
  let expiry: ReturnType<typeof setTimeout>;
  const label = (key: string) => $t(`component.database.${key}`);
  const base = "/api/sys/database";

  async function request(path: string, method: string, payload: object | File, signal: AbortSignal) {
    const binary = payload instanceof File;
    const response = await fetch(base + path, {
      method, signal,
      headers: { "Content-Type": binary ? "application/octet-stream" : "application/json" },
      body: binary ? payload as File : JSON.stringify(payload),
    });
    if (!response.ok) {
      const details = await response.json().catch(() => null);
      throw new Error(details?.error || label("request_failed"));
    }
    return response;
  }
  async function run(task: (signal: AbortSignal) => Promise<void>) {
    const current = new AbortController();
    controller = current; busy = true; error = ""; message = "";
    try { await task(current.signal); }
    catch (e) { if (!current.signal.aborted) error = e.message || label("request_failed"); }
    finally { if (controller === current) { controller = null; busy = false; restoring = false; } }
  }
  function cancelJob() {
    const id = job;
    job = "";
    if (id) fetch(`${base}/jobs/${encodeURIComponent(id)}`, { method: "DELETE", keepalive: true }).catch(() => {});
  }
  function clearSecrets() {
    password = ""; phrase = ""; answers = ["", "", ""]; positions = [];
    files = undefined; preview = undefined; confirmed = false; manualCopy = false;
    clearTimeout(expiry);
  }
  function cancel() {
    if (restoring) return;
    controller?.abort(); controller = null; busy = false;
    cancelJob(); clearSecrets(); step = "idle"; error = ""; message = ""; browse = false;
  }
  function expire() {
    cancel(); error = label("expired");
  }
  function begin(next: "export" | "import") { cancel(); step = next; }
  function download(blob: Blob, name: string) {
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url; link.download = name; link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }
  function prepareExport() {
    run(async signal => {
      const response = await request("/export/prepare", "POST", { password }, signal);
      const prepared = await response.json();
      if (signal.aborted) return;
      job = prepared.id; phrase = prepared.phrase; positions = prepared.positions;
      expiry = setTimeout(expire, 600000); step = "phrase";
    });
  }
  async function copyPhrase() {
    try { await navigator.clipboard.writeText(phrase); message = label("copied"); }
    catch (_) { manualCopy = true; message = label("copy_manual"); }
  }
  function downloadPhrase() {
    download(new Blob([phrase + "\n"], { type: "text/plain;charset=utf-8" }), `oasis-recovery-phrase-${job}.txt`);
  }
  function confirmPhrase() { phrase = ""; manualCopy = false; message = ""; step = "challenge"; }
  function exportBackup() {
    run(async signal => {
      const response = await request(`/export/${job}/download`, "POST", { password, answers }, signal);
      const blob = await response.blob();
      if (signal.aborted) return;
      download(blob, `oasis-database-${job}.oasis-backup`);
      job = ""; clearSecrets(); step = "idle"; message = label("exported");
    });
  }
  function prepareImport() {
    const file = files?.[0];
    if (!file) { error = label("select_file"); return; }
    if (file.size > 64 * 1024 * 1024) { error = label("too_large"); return; }
    run(async signal => {
      const response = await request("/import/prepare", "POST", { password, phrase }, signal);
      const prepared = await response.json();
      if (signal.aborted) return;
      job = prepared.id; phrase = ""; password = "";
      expiry = setTimeout(expire, 600000);
      const uploaded = await request(`/import/${job}`, "PUT", file, signal);
      const result: Preview = await uploaded.json();
      if (signal.aborted) return;
      preview = result; localStorage = result.local_storage; files = undefined; step = "preview";
    });
  }
  function restoreBackup() {
    run(async signal => {
      restoring = true;
      clearTimeout(expiry);
      await request(`/import/${job}/restore`, "POST", { password, local_storage: localStorage, confirm_replace: confirmed }, signal);
      if (signal.aborted) return;
      job = ""; clearSecrets();
      window.location.assign("/login");
    });
  }
  onDestroy(() => {
    // A confirmed restore continues server-side, even if the browser leaves.
    if (!restoring) { controller?.abort(); cancelJob(); }
    clearSecrets();
  });
</script>

<section class="bg-gray-50 shadow rounded-lg p-4 md:p-8 mt-6" aria-labelledby="database-title">
  <h2 id="database-title" class="text-xl font-bold text-gray-700 mb-3">{label("title")}</h2>
  <p class="text-sm mb-4">{label("intro")}</p>
  {#if step === "idle"}
    <div class="flex flex-wrap gap-3">
      <button type="button" class="border rounded px-3 py-2" on:click={() => begin("export")}>{label("export")}</button>
      <button type="button" class="border rounded px-3 py-2" on:click={() => begin("import")}>{label("import")}</button>
    </div>
  {:else if step === "export"}
    <form on:submit|preventDefault={prepareExport}>
      <label class="block mb-3">{label("admin_password")}
        <input required type="password" autocomplete="current-password" maxlength="1024" bind:value={password} disabled={busy} class="block w-full border rounded p-2 mt-1" />
      </label>
      <button class="border rounded px-3 py-2" disabled={busy}>{busy ? label("working") : label("generate")}</button>
    </form>
  {:else if step === "phrase"}
    <h3 class="font-bold mb-2">{label("recovery_phrase")}</h3>
    <p class="text-sm mb-3">{label("phrase_help")}</p>
    <ol class="grid grid-cols-2 md:grid-cols-3 gap-2 mb-4" aria-label={label("recovery_phrase")}>
      {#each phrase.split(" ") as word, index}<li class="border rounded p-2"><span class="text-gray-500 mr-2">{index + 1}.</span>{word}</li>{/each}
    </ol>
    <div class="flex flex-wrap gap-3 mb-3">
      <button type="button" class="border rounded px-3 py-2" on:click={copyPhrase}>{label("copy")}</button>
      <button type="button" class="border rounded px-3 py-2" on:click={downloadPhrase}>{label("download_phrase")}</button>
    </div>
    {#if manualCopy}<textarea readonly aria-label={label("recovery_phrase")} value={phrase} class="w-full border rounded p-2 mb-3" />{/if}
    <button type="button" class="bg-blue-400 text-white rounded px-3 py-2" on:click={confirmPhrase}>{label("recorded")}</button>
  {:else if step === "challenge"}
    <p class="text-sm mb-3">{label("challenge_help")}</p>
    <form on:submit|preventDefault={exportBackup}>
      <fieldset disabled={busy}>
        <div class="grid md:grid-cols-3 gap-3 mb-4">
          {#each positions as position, index}
            <label>{label("word")} {position}
              <input required maxlength="20" autocomplete="off" autocapitalize="none" spellcheck="false" bind:value={answers[index]} class="block w-full border rounded p-2 mt-1" />
            </label>
          {/each}
        </div>
        <button class="bg-blue-400 text-white rounded px-3 py-2">{busy ? label("working") : label("download_backup")}</button>
      </fieldset>
    </form>
  {:else if step === "import"}
    <form on:submit|preventDefault={prepareImport}>
      <fieldset disabled={busy}>
        <label class="block mb-3">{label("backup_file")}
          <input required type="file" accept=".oasis-backup" bind:files class="block w-full mt-1" />
        </label>
        <label class="block mb-3">{label("recovery_phrase")}
          <textarea required maxlength="256" autocomplete="off" autocapitalize="none" spellcheck="false" bind:value={phrase} class="block w-full border rounded p-2 mt-1" />
        </label>
        <label class="block mb-3">{label("admin_password")}
          <input required type="password" autocomplete="current-password" maxlength="1024" bind:value={password} class="block w-full border rounded p-2 mt-1" />
        </label>
        <button class="border rounded px-3 py-2">{busy ? label("working") : label("preview")}</button>
      </fieldset>
    </form>
  {:else if step === "preview"}
    <h3 class="font-bold mb-2">{label("preview")}</h3>
    <dl class="grid grid-cols-2 gap-2 mb-4 text-sm">
      <dt>{label("source_site")}</dt><dd class="break-words">{preview.site_name}</dd>
      <dt>{label("accounts")}</dt><dd>{preview.accounts}</dd>
      <dt>{label("providers")}</dt><dd>{preview.providers}</dd>
      <dt>{label("visibility")}</dt><dd>{preview.visibility_rules}</dd>
      <dt>{label("source_storage")}</dt><dd class="break-words">{preview.source_storage}</dd>
    </dl>
    <p class="text-sm mb-3">{label("migration_help")}</p>
    <p role="alert" class="text-yellow-800 bg-yellow-100 rounded p-3 mb-4">{label("replace_warning")}</p>
    <form on:submit|preventDefault={restoreBackup}>
      <fieldset disabled={busy}>
        <label class="block mb-3">{label("local_storage")}
          <input required bind:value={localStorage} class="block w-full border rounded p-2 mt-1" />
        </label>
        <button type="button" class="border rounded px-3 py-2 mb-3" on:click={() => browse = true}>{label("browse")}</button>
        <label class="block mb-3">{label("admin_password")}
          <input required type="password" autocomplete="current-password" maxlength="1024" bind:value={password} class="block w-full border rounded p-2 mt-1" />
        </label>
        <label class="block mb-4"><input required type="checkbox" bind:checked={confirmed} /> {label("confirm_replace")}</label>
        <button class="bg-red-700 text-white rounded px-3 py-2">{busy ? label("restoring") : label("restore")}</button>
      </fieldset>
    </form>
  {/if}
  {#if step !== "idle"}<button type="button" disabled={restoring} class="underline mt-4" on:click={cancel}>{label("cancel")}</button>{/if}
  {#if error}<p role="alert" class="text-red-700 mt-4 break-words">{error}</p>{/if}
  {#if message}<p role="status" class="text-green-700 mt-4">{message}</p>{/if}
</section>
{#if browse}<DirBrowser onClose={() => browse = false} onSelect={(path) => { localStorage = path; browse = false; }} />{/if}
