const field = document.getElementById("code");
const boxes = field && field.closest(".pin");
let sent = false;

function settle(submit) {
  const clean = field.value.toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 8);
  if (clean !== field.value) field.value = clean;
  boxes.dataset.length = String(clean.length);
  if (submit && clean.length === 8 && !sent) field.form.requestSubmit();
}

if (boxes) {
  const button = field.form.querySelector("button");
  settle(false);
  field.addEventListener("input", () => {
    field.form.classList.remove("wrong");
    field.setAttribute("aria-invalid", "false");
    const feedback = document.getElementById("code-feedback");
    if (feedback) {
      feedback.className = "feedback hint";
      feedback.textContent = "Lowercase and dashes are fine.";
    }
    settle(true);
  });
  field.form.addEventListener("submit", (event) => {
    if (sent) return event.preventDefault();
    sent = true;
    button.disabled = true;
    button.textContent = "Connecting…";
  });
  window.addEventListener("pageshow", (event) => {
    if (!event.persisted) return;
    sent = false;
    button.disabled = false;
    button.textContent = "Connect";
  });
}
