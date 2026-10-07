(function () {
  // Copy button on code blocks (docs, guides); the homepage's terminal demos are skipped.
  if (!navigator.clipboard) return;
  document.querySelectorAll('pre').forEach(function (pre) {
    if (pre.closest('.terminal') || pre.querySelector('.pre-copy')) return;
    pre.classList.add('has-copy');
    var b = document.createElement('button');
    b.type = 'button'; b.className = 'pre-copy'; b.textContent = 'Copy';
    b.addEventListener('click', function () {
      navigator.clipboard.writeText(pre.innerText.replace(/\s*Cop(y|ied)$/, '')).then(function () {
        b.textContent = 'Copied'; setTimeout(function () { b.textContent = 'Copy'; }, 1400);
      });
    });
    pre.appendChild(b);
  });
})();
