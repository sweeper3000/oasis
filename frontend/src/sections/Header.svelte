<script lang="ts">
  import { onDestroy, tick } from "svelte";
  import { t } from "svelte-i18n";
  import { siteStore, sectionStore, getSitename } from "../utils/store";
  import AvatarMenu from "./AvatarMenu.svelte";
  import Icon from "../components/Icon.svelte";
  import Title from "./Title.svelte";
  import { EIconColor, EIconType } from "../utils/enums";
  import type { IUpdateAppInfo } from "../utils/types";
  import SignOutModal from "../modals/SignOutModal.svelte";
  import ShutdownModal from "../modals/ShutdownModal.svelte";
  import AboutModal from "../modals/AboutModal.svelte";
  import UpdateModal from "../modals/UpdateModal.svelte";
  import SearchModal from "../modals/SearchModal.svelte";

  import { getSearchShortcut, isSearchShortcut } from "../utils/keyboard";

  const shortcut = getSearchShortcut(navigator.platform || navigator.userAgent);
  let searchButton: HTMLButtonElement;
  let searchDialog: SearchModal;
  let hasSearch = false;

  $: hasSearch = section === "files" || section === "settings" || section === "profile";

  let sitename = getSitename();
  let section: string;
  let updateInfo: IUpdateAppInfo;
  let showMenu = false;
  let showSignOutModal = false;
  let showShutdownModal = false;
  let showAboutModal = false;
  let showUpdateModal = false;
  let showSearchModal = false;

  const unsubscribeSection = sectionStore.subscribe((newSection) => {
    if (newSection) section = newSection;
  });

  const unsubscribeSite = siteStore.subscribe((newSite) => {
    if (newSite && newSite.name !== sitename) {
      sitename = newSite.name;
    }
  });

  onDestroy(() => {
    unsubscribeSection();
    unsubscribeSite();
  });

  $: if (showMenu) {
    document.addEventListener("click", closeAvatarMenu, false);
  } else {
    document.removeEventListener("click", closeAvatarMenu);
  }

  $: if (updateInfo) {
    showAboutModal = false;
    showUpdateModal = true;
  }

  const openAvatarMenu = (e: Event) => {
    e.stopPropagation();
    showMenu = true;
  };

  const openModal = (name: string) => {
    showMenu = false;
    switch (name) {
      case "signout":
        showSignOutModal = true;
        break;
      case "shutdown":
        showShutdownModal = true;
        break;
      case "about":
        showAboutModal = true;
        break;
      default:
        break;
    }
  };

  const closeAvatarMenu = () => {
    showMenu = false;
  };

  const closeSignOutModal = () => {
    showSignOutModal = false;
  };

  const clostShutdownModal = () => {
    showShutdownModal = false;
  };

  const closeAboutModal = () => {
    showAboutModal = false;
  };

  const setUpdateInfo = (info: IUpdateAppInfo) => {
    updateInfo = info;
  };

  const closeUpdateModal = () => {
    showUpdateModal = false;
  };

  const closeSearchModal = async () => {
    showSearchModal = false;
    await tick();
    if (searchButton?.isConnected) searchButton.focus();
  };
  const hasOtherModal = () => Array.from(document.querySelectorAll("[data-oasis-modal]"))
    .some((modal) => !modal.querySelector("#oasis-search-input"));

  const openSearch = async () => {
    if (!hasSearch || hasOtherModal()) return;
    showMenu = false;
    showSearchModal = true;
    await tick();
    searchDialog?.focusSearch();
  };

  const handleSearchShortcut = (event: KeyboardEvent) => {
    if (!hasSearch || hasOtherModal() || !isSearchShortcut(event, shortcut.isApple)) return;
    event.preventDefault();
    openSearch();
  };

</script>

<svelte:window on:keydown={handleSearchShortcut} />

{#if showSignOutModal}
  <SignOutModal onClose={closeSignOutModal} />
{/if}
{#if showShutdownModal}
  <ShutdownModal onClose={clostShutdownModal} />
{/if}
{#if showAboutModal}
  <AboutModal onClose={closeAboutModal} {setUpdateInfo} />
{/if}
{#if showUpdateModal}
  <UpdateModal onClose={closeUpdateModal} {updateInfo} />
{/if}
{#if showSearchModal}
  <SearchModal bind:this={searchDialog} onClose={closeSearchModal} />
{/if}
<div class="w-full h-14 bg-gray-50 dark:bg-gray-800 shadow">
  <div
    class="w-11/12 lg:w-4/5 h-full flex flex-row justify-between items-center mx-auto"
  >
    <Title {sitename} {section} />
    {#if hasSearch}
      <button
        type="button"
        bind:this={searchButton}
        class="flex flex-row w-40 lg:w-96 items-center text-left border-b pb-1 border-gray-300 dark:border-gray-600 cursor-pointer focus:ring-2 focus:ring-blue-400 rounded-sm"
        aria-label={$t("modal.search_file.open_search")}
        aria-keyshortcuts={shortcut.aria}
        aria-haspopup="dialog"
        on:click={openSearch}
      >
        <span class="flex-shrink-0"><Icon type={EIconType.search} size="tiny" color={EIconColor.gray} /></span>
        <span class="ml-2 min-w-0 truncate">{$t("modal.search_file.placeholder")}</span>
        <kbd class="ml-auto pl-2 flex-shrink-0 text-xs whitespace-nowrap"><span class="border border-gray-300 dark:border-gray-600 rounded px-1 py-0.5 bg-white dark:bg-gray-800">{shortcut.label}</span></kbd>
      </button>
      <div class="relative">
        <Icon
          type={EIconType.profile}
          onClick={openAvatarMenu}
          className="cursor-pointer"
        />
        {#if showMenu}
          <AvatarMenu onClose={closeAvatarMenu} onOpenModal={openModal} />
        {/if}
      </div>
    {/if}
  </div>
</div>
