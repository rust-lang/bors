<script setup lang="ts">
import { ref } from "vue";
import { formatDurationMs, formatRemainingMs, formatTimestamp } from "../time";

const BUILDS_TABLE_SHOWN_KEY = "prefs.builds-table.shown";
const MAX_JOBS_COLUMN = 5;
const MAX_JOBS_TITLE = 20;

defineProps<{
  builds: PendingBuild[];
  repoUrl: string;
  averageBuildDurationMs: number;
  nowMs: number;
}>();

const emit = defineEmits<{
  hoverPr: [prNumber: number | null];
}>();

function loadShownPreference(): boolean {
  try {
    return window.localStorage.getItem(BUILDS_TABLE_SHOWN_KEY) !== "false";
  } catch {
    return true;
  }
}

const isShown = ref(loadShownPreference());

function toggleShown() {
  isShown.value = !isShown.value;
  try {
    window.localStorage.setItem(BUILDS_TABLE_SHOWN_KEY, JSON.stringify(isShown.value));
  } catch {
  }
}

function formatJobsSummary(workflow: PendingWorkflow): string {
  const base = `${workflow.completed_jobs}/${workflow.total_jobs}`;
  if (workflow.remaining_jobs.length === 0) {
    return base;
  }

  const shown = workflow.remaining_jobs.slice(0, MAX_JOBS_COLUMN).join(", ");
  const suffix = workflow.remaining_jobs.length > MAX_JOBS_COLUMN ? ", ..." : "";
  return `${base} (${shown}${suffix})`;
}

function formatJobsTitle(workflow: PendingWorkflow): string {
  const header = "Number of completed/started jobs. Note that this can be inaccurate!";
  if (workflow.remaining_jobs.length === 0) {
    return header;
  }

  const lines = workflow.remaining_jobs.slice(0, MAX_JOBS_TITLE);
  const leftover = workflow.remaining_jobs.length - MAX_JOBS_TITLE;
  if (leftover > 0) {
    lines.push(`(and ${leftover} other${leftover === 1 ? "" : "s"})`);
  }

  return `${header}\n\nRemaining jobs (${workflow.remaining_jobs.length}):\n${lines.join("\n")}`;
}
</script>

<template>
  <template v-if="builds.length > 0">
    <div class="toggle">
      <button type="button" title="Toggle pending builds table" @click="toggleShown">
        Pending builds
      </button>
    </div>

    <div v-show="isShown" class="table-builds-wrapper">
      <table class="table-builds">
        <thead>
          <tr>
            <th class="num-col">#</th>
            <th class="num-col">Workflow URL</th>
            <th class="num-col">Started at</th>
            <th class="num-col">Elapsed</th>
            <th class="num-col">Remaining (est.)</th>
            <th class="jobs-col">Jobs</th>
          </tr>
        </thead>
        <tbody @mouseleave="emit('hoverPr', null)">
          <tr
            v-for="build in builds"
            :key="build.pr_number"
            @mouseenter="emit('hoverPr', build.pr_number)"
          >
            <td class="num-col">
              <a :href="`${repoUrl}/pull/${build.pr_number}`">{{ build.pr_number }}</a>
            </td>
            <td class="num-col">
              <a v-if="build.workflow" :href="build.workflow.url">
                {{ build.workflow.run_id }}
              </a>
            </td>
            <td class="num-col">{{ formatTimestamp(build.created_at_ms) }}</td>
            <td class="num-col">{{ formatDurationMs(nowMs - build.created_at_ms) }}</td>
            <td class="num-col">
              {{ formatRemainingMs(averageBuildDurationMs - (nowMs - build.created_at_ms)) }}
            </td>
            <td
              class="job-list"
              :title="build.workflow ? formatJobsTitle(build.workflow) : undefined"
            >
              <template v-if="build.workflow">
                {{ formatJobsSummary(build.workflow) }}
              </template>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </template>
</template>

<style scoped>
.table-builds-wrapper {
  max-width: 1200px;
  overflow-x: auto;
  margin-bottom: 0.75rem;
}

.table-builds {
  min-width: 100%;
  white-space: nowrap;
}

.table-builds th,
.table-builds td {
  padding: 6px 8px;
}

.table-builds thead th {
  border-bottom: 1px solid rgba(0, 0, 0, 0.3);
}

.table-builds tbody tr:nth-child(odd) {
  background-color: rgb(238, 238, 238);
}

.table-builds tbody tr:hover {
  background-color: #e4e9f5;
}

.num-col {
  text-align: right;
}

.jobs-col {
  width: 100%;
}

.job-list {
  max-width: 1000px;
  overflow-x: hidden;
  text-overflow: ellipsis;
}
</style>
