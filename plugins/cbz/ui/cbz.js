// .cbz comic viewer: a cbz is a zip of images. Unzip with JSZip, page through.
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
  const IMG = /\.(jpe?g|png|gif|webp|avif)$/i;

  (async () => {
    await loadScript("/plugins/cbz/ui/vendor/jszip.min.js");
    window.Shelfarrs.registerViewer({
      id: "cbz",
      formats: ["cbz"],
      async mount(el, ctx) {
        const buf = await fetch(ctx.fileUrl).then((r) => r.arrayBuffer());
        const zip = await JSZip.loadAsync(buf);
        const names = Object.keys(zip.files)
          .filter((n) => IMG.test(n) && !zip.files[n].dir)
          .sort();
        if (!names.length) {
          el.innerHTML = "<p style='color:#ccc;padding:2rem'>No images in this archive.</p>";
          return;
        }
        const img = document.createElement("img");
        img.className = "cbz-page";
        el.appendChild(img);

        let i = (ctx.progress && ctx.progress.page) | 0;
        let url = null;
        async function show(n) {
          i = Math.max(0, Math.min(names.length - 1, n));
          const blob = await zip.files[names[i]].async("blob");
          if (url) URL.revokeObjectURL(url);
          url = URL.createObjectURL(blob);
          img.src = url;
          el.scrollTop = 0;
          ctx.onProgress({ page: i, percent: (i + 1) / names.length });
        }
        const next = () => show(i + 1);
        const prev = () => show(i - 1);
        document.getElementById("next").onclick = next;
        document.getElementById("prev").onclick = prev;
        document.addEventListener("keyup", (e) => {
          if (e.key === "ArrowRight") next();
          if (e.key === "ArrowLeft") prev();
        });
        show(i);
      },
    });
  })();
})();
