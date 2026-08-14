(function () {
  var t = localStorage.getItem("learnive-theme");
  if (t !== "light" && t !== "dark") {
    t = matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  }
  document.documentElement.dataset.theme = t;
})();
