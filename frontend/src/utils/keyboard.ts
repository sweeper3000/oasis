export function getSearchShortcut(platform: string) {
  const isApple = /Mac|iPhone|iPad|iPod/i.test(platform);
  return {
    isApple,
    label: isApple ? "⌘K" : "Ctrl+K",
    aria: isApple ? "Meta+K" : "Control+K",
  };
}

export function isSearchShortcut(event: KeyboardEvent, isApple: boolean): boolean {
  return !event.defaultPrevented && !event.repeat && !event.isComposing &&
    !event.altKey && !event.shiftKey && event.key.toLowerCase() === "k" &&
    (isApple ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey);
}
