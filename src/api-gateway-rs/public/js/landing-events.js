(function() {
  window.dataLayer = window.dataLayer || [];
  document.querySelectorAll('[data-gtm-event]').forEach(function (el) {
    el.addEventListener('click', function () {
      window.dataLayer.push({ event: el.dataset.gtmEvent, cta_label: el.dataset.gtmLabel });
    });
  });
})();
