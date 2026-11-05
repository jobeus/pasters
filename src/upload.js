// Uploads pasted, dropped or picked content to POST / with a progress bar, then opens the paste.
// Shared by the homepage and the web form; `data-page` on <body> says which one this is.
(() => {
  const body = document.body;
  const page = body.dataset.page; // "home" or "web"
  const maxText = Number(body.dataset.maxText);
  const maxImage = Number(body.dataset.maxImage);
  const IMAGE_TYPES = ["image/png", "image/jpeg"];

  const box = document.createElement("div");
  box.id = "upload";
  box.hidden = true;
  box.innerHTML = '<div class="bar"><div class="fill"></div></div><div class="label" role="status"></div>';
  document.body.append(box);
  const fill = box.querySelector(".fill");
  const label = box.querySelector(".label");
  let busy = false;

  const kib = (n) => (n / 1024).toFixed(n < 10240 ? 1 : 0);
  const mib = (n) => (n / 1048576).toFixed(1);

  function show(text, frac, state) {
    box.hidden = false;
    box.dataset.state = state || "busy";
    label.textContent = text;
    fill.style.width = `${Math.round(Math.min(1, Math.max(0, frac)) * 100)}%`;
  }
  function fail(text) {
    busy = false;
    show(text, 1, "error");
  }
  box.addEventListener("click", () => {
    if (!busy) box.hidden = true;
  });

  // `action` is "paste", "drop" or "upload"; it is sent (with the page) so the server can log
  // where uploads come from. Nothing else about the content is reported.
  function upload(blob, action) {
    if (busy) return;
    const isImage = IMAGE_TYPES.includes(blob.type);
    if (blob.type.startsWith("image/") && !isImage) {
      return fail(`Only PNG and JPEG images are supported (got ${blob.type}).`);
    }
    if (isImage && blob.size > maxImage) {
      return fail(`Image is ${mib(blob.size)} MiB; the limit is ${mib(maxImage)} MiB.`);
    }
    if (!isImage && blob.size > maxText) {
      return fail(`Text is ${kib(blob.size)} KiB; the limit is ${kib(maxText)} KiB.`);
    }
    if (blob.size === 0) return fail("Nothing to paste.");

    busy = true;
    const what = isImage ? "image" : "text";
    show(`Uploading ${what}…`, 0);
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/");
    xhr.setRequestHeader("X-Paste-Client", `${page}-${action}`);
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) {
        const size = isImage ? `${mib(e.loaded)} / ${mib(e.total)} MiB` : `${kib(e.loaded)} / ${kib(e.total)} KiB`;
        show(`Uploading ${what}… ${size}`, e.loaded / e.total);
      }
    };
    xhr.upload.onload = () => show("Saving…", 1);
    xhr.onload = () => {
      const text = xhr.responseText.trim();
      if (xhr.status === 201 || xhr.status === 206) {
        show("Done, opening…", 1, "done");
        location.href = text;
      } else {
        fail(text || `Upload failed (HTTP ${xhr.status}).`);
      }
    };
    xhr.onerror = () => fail("Upload failed: network error.");
    xhr.send(blob);
  }

  const imageIn = (items) => [...(items || [])].find((f) => f.type.startsWith("image/"));

  document.addEventListener("paste", (e) => {
    const data = e.clipboardData;
    if (!data) return;
    const image = imageIn(data.files);
    if (image) {
      e.preventDefault();
      return upload(image, "paste");
    }
    // On the web form, text pastes go into the textarea as usual.
    if (page !== "home") return;
    const text = data.getData("text/plain");
    if (text.trim()) {
      e.preventDefault();
      upload(new Blob([text], { type: "text/plain;charset=utf-8" }), "paste");
    }
  });

  // Dropping a file anywhere on the page uploads it.
  let depth = 0;
  const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes("Files");
  document.addEventListener("dragenter", (e) => {
    if (hasFiles(e) && depth++ === 0) body.classList.add("dragging");
  });
  document.addEventListener("dragleave", (e) => {
    if (hasFiles(e) && --depth === 0) body.classList.remove("dragging");
  });
  document.addEventListener("dragover", (e) => {
    if (hasFiles(e)) e.preventDefault();
  });
  document.addEventListener("drop", (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    depth = 0;
    body.classList.remove("dragging");
    const file = e.dataTransfer.files[0];
    if (file) upload(file, "drop");
  });

  const picker = document.getElementById("file");
  if (picker) {
    picker.addEventListener("change", () => {
      const file = picker.files[0];
      picker.value = "";
      if (file) upload(file, "upload");
    });
  }
})();
