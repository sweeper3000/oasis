/* Runs before styles load; the store uses this same controller after mounting. */
(function () {
  var key = "oa_theme";
  var preference = "system";
  var media = window.matchMedia("(prefers-color-scheme: dark)");
  var listeners = [];

  function normalize(value) {
    return value === "light" || value === "dark" ? value : "system";
  }

  function apply() {
    var dark = preference === "dark" || (preference === "system" && media.matches);
    document.documentElement.classList.toggle("dark", dark);
    document.documentElement.style.colorScheme = dark ? "dark" : "light";
  }

  function update(value) {
    preference = normalize(value);
    apply();
    listeners.slice().forEach(function (listener) { listener(preference); });
  }

  try { preference = normalize(localStorage.getItem(key)); } catch (_) {}
  apply();
  if (media.addEventListener) media.addEventListener("change", apply);
  else media.addListener(apply);

  window.addEventListener("storage", function (event) {
    if (event.key === key || event.key === null) update(event.newValue);
  });

  window.oasisTheme = {
    subscribe: function (listener) {
      listeners.push(listener);
      listener(preference);
      return function () { listeners = listeners.filter(function (item) { return item !== listener; }); };
    },
    set: function (value) {
      update(value);
      try { localStorage.setItem(key, preference); } catch (_) {}
    }
  };
}());
