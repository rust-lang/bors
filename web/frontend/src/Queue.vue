<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import PendingBuildsTable from "./components/PendingBuildsTable.vue";
import QueueTable from "./components/QueueTable.vue";
import RollupModal from "./components/RollupModal.vue";
import { formatDurationMs } from "./time";

const props = defineProps<{
  data: Data;
}>();

const nowMs = ref(Date.now());
let timerId: number | undefined;

const selectedPrNumbers = ref<number[]>([]);
const hoveredBuildPr = ref<number | null>(null);
const rollupModalOpen = ref(false);

const expectedFlushText = computed(() => {
  if (props.data.expected_remaining_duration_ms === null) {
    return null;
  }
  return formatDurationMs(props.data.expected_remaining_duration_ms);
});

const flushTitle = computed(() => {
  const avg = formatDurationMs(props.data.average_build_duration_ms);
  return (
    "Estimated duration to merge all current PRs in the queue, assuming that there are no build failures.\n\n" +
    `Average duration of recent auto builds: ${avg}`
  );
});

function openRollupModal() {
  if (props.data.oauth_client_id === null) {
    alert("Both CLIENT_ID and CLIENT_SECRET must be set to enabled OAuth.");
    return;
  }
  rollupModalOpen.value = true;
}

onMounted(() => {
  if (props.data.pending_builds.length > 0) {
    timerId = window.setInterval(() => {
      nowMs.value = Date.now();
    }, 1000);
  }
});

onUnmounted(() => {
  if (timerId !== undefined) {
    window.clearInterval(timerId);
  }
});
</script>

<template>
  <nav>
    <h1 class="queue-title">
      Bors queue - <a :href="data.repo_url" target="_blank">{{ data.repo_name }}</a>
      <template v-if="data.tree_closed">
        [<a :href="data.tree_closed.comment_source">TREECLOSED</a> below priority
        {{ data.tree_closed.priority
        }}<template v-if="data.tree_closed.reason"> ({{ data.tree_closed.reason }})</template>]
      </template>
    </h1>
    <div>
      <template v-if="data.ec2_configured">
        <a :href="`/ec2/${data.repo_owner}/${data.repo_name}`">EC2 instances</a> |
      </template>
      <a href="/help">Help</a>
    </div>
  </nav>

  <div class="queue-summary">
    <button type="button" @click="openRollupModal">Create rollup</button>
    <p class="queue-stats">
      {{ data.stats.total_count }} total, {{ data.stats.in_queue_count }} in queue,
      {{ data.stats.failed_count }} failed
    </p>
    <template v-if="expectedFlushText !== null">
      <div class="separator">|</div>
      <p class="queue-flush" :title="flushTitle">
        Estimated time to flush queue: {{ expectedFlushText }}
      </p>
    </template>
  </div>

  <PendingBuildsTable
    :builds="data.pending_builds"
    :repo-url="data.repo_url"
    :average-build-duration-ms="data.average_build_duration_ms"
    :now-ms="nowMs"
    @hover-pr="hoveredBuildPr = $event"
  />

  <QueueTable
    :prs="data.prs"
    :pending-builds="data.pending_builds"
    :initial-selected-prs="data.selected_rollup_prs"
    :repo-url="data.repo_url"
    :tree-closed-priority="data.tree_closed?.priority ?? null"
    :average-build-duration-ms="data.average_build_duration_ms"
    :now-ms="nowMs"
    :hovered-build-pr="hoveredBuildPr"
    @selected-change="selectedPrNumbers = $event"
  />

  <RollupModal
    :open="rollupModalOpen"
    :selected-pr-numbers="selectedPrNumbers"
    :repo-name="data.repo_name"
    :repo-owner="data.repo_owner"
    :oauth-client-id="data.oauth_client_id"
    @close="rollupModalOpen = false"
  />
</template>

<style scoped>
nav {
  margin-bottom: 10px;
  display: flex;
  flex-direction: row;
  justify-content: space-between;
  align-items: center;
}

.queue-title {
  margin-bottom: 0;
}

.queue-summary {
  margin-bottom: 1rem;
  display: flex;
  align-items: center;
}

.queue-stats {
  margin-left: 10px;
  margin-bottom: 0;
}

.separator {
  margin: 0 5px;
}

.queue-flush {
  margin-bottom: 0;
}
</style>

<style>
.data-table {
  min-width: 100%;
  white-space: nowrap;
}

.data-table th,
.data-table td {
  padding: 6px 8px;
}

.data-table thead th {
  border-bottom: 1px solid rgba(0, 0, 0, 0.3);
}

.data-table tbody tr:nth-child(odd) {
  background-color: rgb(238, 238, 238);
}

.data-table .num-col {
  text-align: right;
}
</style>
