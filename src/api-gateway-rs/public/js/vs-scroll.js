(function() {
  var navEl = document.querySelector('header.nav');
  var progressEl = document.getElementById('scroll-progress');
  function onScroll() {
    if (navEl) navEl.classList.toggle('scrolled', window.scrollY > 8);
    if (progressEl) {
      var scrollable = document.documentElement.scrollHeight - window.innerHeight;
      var pct = scrollable > 0 ? (window.scrollY / scrollable) * 100 : 0;
      progressEl.style.width = pct + '%';
    }
  }
  document.addEventListener('scroll', onScroll, { passive: true });
  onScroll();
})();
