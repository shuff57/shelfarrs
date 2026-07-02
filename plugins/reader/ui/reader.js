// Flagship epub viewer. Self-registers against window.Shelfarr and renders with
// epub.js. Pulls its own vendored deps so the core /read page stays generic.
(function () {
  function loadScript(src) {
    return new Promise((res, rej) => {
      const s = document.createElement("script");
      s.src = src;
      s.onload = res;
      s.onerror = () => rej(new Error("failed to load " + src));
      document.head.appendChild(s);
    });
  }

  (async () => {
    await loadScript("/plugins/reader/ui/vendor/jszip.min.js");
    await loadScript("/plugins/reader/ui/vendor/epub.min.js");

    window.Shelfarr.registerViewer({
      id: "reader",
      formats: ["epub"],
      async mount(el, ctx) {
        // Hand epub.js the raw bytes as an ArrayBuffer so it opens them as a zip
        // archive — /books/:id/file has no .epub suffix for it to sniff.
        const buf = await fetch(ctx.fileUrl).then((r) => r.arrayBuffer());
        const book = ePub(buf);
        const rendition = book.renderTo(el, {
          width: "100%",
          height: "100%",
          flow: "paginated",
          spread: "auto",
        });

        const saved = ctx.progress && ctx.progress.cfi ? ctx.progress.cfi : undefined;
        rendition.display(saved);

        rendition.on("relocated", (loc) => {
          ctx.onProgress({ cfi: loc.start.cfi, percent: loc.start.percentage });
        });

        const prev = () => rendition.prev();
        const next = () => rendition.next();
        document.addEventListener("keyup", (e) => {
          if (e.key === "ArrowRight") next();
          if (e.key === "ArrowLeft") prev();
        });
        document.getElementById("prev").onclick = prev;
        document.getElementById("next").onclick = next;
      },
    });
  })();
})();
