<script setup lang="ts">
import { ref, watch } from "vue";

const props = defineProps<{
  open: boolean;
  selectedPrNumbers: number[];
  repoName: string;
  repoOwner: string;
  oauthClientId: string | null;
}>();

const emit = defineEmits<{
  close: [];
}>();

const dialogRef = ref<HTMLDialogElement | null>(null);

watch(
  () => props.open,
  (isOpen) => {
    const dialog = dialogRef.value;
    if (!dialog) {
      return;
    }
    if (isOpen && !dialog.open) {
      dialog.showModal();
    } else if (!isOpen && dialog.open) {
      dialog.close();
    }
  },
);

function handleContinue() {
  if (!props.oauthClientId) {
    return;
  }

  const state = JSON.stringify({
    pr_nums: props.selectedPrNumbers,
    repo_name: props.repoName,
    repo_owner: props.repoOwner,
  });

  const oauthUrl = new URL("https://github.com/login/oauth/authorize");
  oauthUrl.searchParams.set("client_id", props.oauthClientId);
  oauthUrl.searchParams.set("scope", "public_repo,workflow");
  oauthUrl.searchParams.set("state", state);

  window.location.href = oauthUrl.toString();
}
</script>

<template>
  <dialog
    ref="dialogRef"
    class="rollup-modal"
    @close="emit('close')"
    @click.self="emit('close')"
  >
    <div class="rollup-modal-content">
      <p v-if="selectedPrNumbers.length === 0">No PRs selected for rollup.</p>
      <template v-else>
        <p>
          You've selected <strong>{{ selectedPrNumbers.length }} PR(s)</strong> to be included in
          this rollup.
        </p>
        <p>
          A rollup is useful for shortening the queue, but jumping the queue is unfair to older PRs
          who have waited too long.
        </p>
        <p>
          When creating a real rollup, see the
          <a href="https://forge.rust-lang.org/release/rollups.html" target="_blank">
            instructions
          </a>
          for reference.
        </p>
        <button type="button" @click="handleContinue">Continue</button>
      </template>
    </div>
  </dialog>
</template>

<style scoped>
.rollup-modal {
  margin: 15% auto;
  padding: 0;
  border: 1px solid var(--color-rollup-modal-border);
  max-width: 500px;
  width: calc(100% - 2rem);
  background-color: var(--color-rollup-modal);
}

.rollup-modal::backdrop {
  background-color: var(--color-rollup-modal-fade);
}

.rollup-modal-content {
  padding: 1rem;
}
</style>
