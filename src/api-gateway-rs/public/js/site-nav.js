(function () {
  var nav = document.currentScript.previousElementSibling, btn = nav.querySelector('.sn-menu');
  btn.addEventListener('click', function () {
    var open = !nav.hasAttribute('data-open');
    if (open) nav.setAttribute('data-open', ''); else nav.removeAttribute('data-open');
    btn.setAttribute('aria-expanded', String(open));
  });
  nav.querySelectorAll('.sn-panel a').forEach(function (a) {
    a.addEventListener('click', function () { nav.removeAttribute('data-open'); btn.setAttribute('aria-expanded', 'false'); });
  });
})();
