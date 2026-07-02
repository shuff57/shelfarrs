// PDF viewer via pdf.js. Renders one page to a canvas, pages with prev/next.
(function () {
  function loadScript(src) {
    return new Promise((res, rej) => {
      const s = document.createElement("script");
      s.src = src;
      s.onload = res;
      s.onerror = () => rej(new Error("load " + src));
      document.head.appendChild(s);
    });
  }

  (async () => {
    await loadScript("/plugins/pdf/ui/vendor/pdf.min.js");
    const pdfjs = window.pdfjsLib;
    pdfjs.GlobalWorkerOptions.workerSrc = "/plugins/pdf/ui/vendor/pdf.worker.min.js";

    window.Shelfarrs.registerViewer({
      id: "pdf",
      formats: ["pdf"],
      async mount(el, ctx) {
        const buf = await fetch(ctx.fileUrl).then((r) => r.arrayBuffer());
        const doc = await pdfjs.getDocument({ data: new Uint8Array(buf) }).promise;
        const canvas = document.createElement("canvas");
        el.appendChild(canvas);
        const gctx = canvas.getContext("2d");

        let n = (ctx.progress && ctx.progress.page) || 1;
        async function render(p) {
          n = Math.max(1, Math.min(doc.numPages, p));
          const page = await doc.getPage(n);
          // Fit width to the viewport.
          const base = page.getViewport({ scale: 1 });
          const scale = Math.min(2, (el.clientWidth - 20) / base.width);
          const vp = page.getViewport({ scale });
          canvas.width = vp.width;
          canvas.height = vp.height;
          await page.render({ canvasContext: gctx, viewport: vp }).promise;
          el.scrollTop = 0;
          ctx.onProgress({ page: n, percent: n / doc.numPages });
        }
        const next = () => render(n + 1);
        const prev = () => render(n - 1);
        document.getElementById("next").onclick = next;
        document.getElementById("prev").onclick = prev;
        document.addEventListener("keyup", (e) => {
          if (e.key === "ArrowRight") next();
          if (e.key === "ArrowLeft") prev();
        });
        render(n);
      },
    });
  })();
})();
