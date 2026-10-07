  // Highlights the sidebar link for whichever section is currently in
  // view, same IntersectionObserver pattern used elsewhere on the site,
  // just for nav state instead of a reveal animation.
  (function() {
    var sections = document.querySelectorAll('.docs-content > section[id]');
    var links = document.querySelectorAll('.docs-sidebar a');
    function setActive(id) {
      links.forEach(function(a) { a.classList.toggle('active', a.getAttribute('href') === '#' + id); });
    }
    var observer = new IntersectionObserver(function(entries) {
      entries.forEach(function(entry) {
        if (entry.isIntersecting) setActive(entry.target.id);
      });
    }, { rootMargin: '-80px 0px -70% 0px' });
    sections.forEach(function(s) { observer.observe(s); });
  })();
