<script setup lang="ts">
import { computed, ref, watch } from "vue";
import {
  columnFilteringFeature,
  columnGroupingFeature,
  createColumnHelper,
  createExpandedRowModel,
  createFilteredRowModel,
  createGroupedRowModel,
  createPaginatedRowModel,
  createSortedRowModel,
  globalFilteringFeature,
  rowExpandingFeature,
  rowPaginationFeature,
  rowSelectionFeature,
  rowSortingFeature,
  sortFn_alphanumeric,
  sortFn_basic,
  tableFeatures,
  useTable,
  type FilterFn,
  type Row,
  type RowSelectionState,
} from "@tanstack/vue-table";
import { calculateProgressPct, formatDurationMs, formatTimestamp } from "../time";

const DEFAULT_PAGE_SIZE = 75;
type PageItem = number | "ellipsis";

const props = defineProps<{
  prs: PullRequest[];
  pendingBuilds: PendingBuild[];
  initialSelectedPrs: number[];
  repoUrl: string;
  treeClosedPriority: number | null;
  averageBuildDurationMs: number;
  nowMs: number;
  hoveredBuildPr: number | null;
}>();

const emit = defineEmits<{
  selectedChange: [prNumbers: number[]];
}>();

const features = tableFeatures({
  rowSelectionFeature,
  columnFilteringFeature,
  globalFilteringFeature,
  filteredRowModel: createFilteredRowModel(),
  columnGroupingFeature,
  groupedRowModel: createGroupedRowModel(),
  rowSortingFeature,
  sortedRowModel: createSortedRowModel(),
  sortFns: {
    alphanumeric: sortFn_alphanumeric,
    basic: sortFn_basic,
  },
  rowExpandingFeature,
  expandedRowModel: createExpandedRowModel(),
  rowPaginationFeature,
  paginatedRowModel: createPaginatedRowModel(),
});

// This is a special note added by rustc-perf when it performs a try benchmark on a PR
// and the result is significant.
function hasSignificantPerf(pr: PullRequest): boolean {
  return pr.note === "rustc-perf";
}

function hasNoteStar(pr: PullRequest): boolean {
  return Boolean(pr.note && !hasSignificantPerf(pr));
}

function formatAssignees(pr: PullRequest): string {
  return pr.assignees.join(", ");
}

function isRollup(pr: PullRequest): boolean {
  return pr.rollup_members.length > 0;
}

const pendingByPr = computed(
  () => new Map(props.pendingBuilds.map((build) => [build.pr_number, build])),
);

const columnHelper = createColumnHelper<typeof features, PullRequest>();

const columns = columnHelper.columns([
  columnHelper.display({
    id: "select",
  }),
  columnHelper.accessor("number", {
    header: "#",
    sortFn: "basic",
    sortDescFirst: false,
  }),
  columnHelper.accessor("status", {
    header: "Status",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor("mergeable", {
    header: "Mergeable",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor("title", {
    header: "Title",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor("author", {
    header: "Author",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor(formatAssignees, {
    id: "assignees",
    header: "Assignees",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor((pr) => pr.approver ?? "", {
    id: "approved_by",
    header: "Approved by",
    sortFn: "alphanumeric",
  }),
  columnHelper.accessor("priority", {
    header: "Priority",
    sortFn: "basic",
    sortDescFirst: false,
  }),
  columnHelper.accessor((pr) => pr.rollup ?? "", {
    id: "rollup",
    header: "Rollup",
    sortFn: "alphanumeric",
  }),
]);

const rollupableNumbers = computed(
  () => new Set(props.prs.filter((pr) => pr.is_rollupable).map((pr) => pr.number)),
);

const initialRowSelection: RowSelectionState = {};
for (const num of props.initialSelectedPrs) {
  if (rollupableNumbers.value.has(num)) {
    initialRowSelection[String(num)] = true;
  }
}

function handleGroupByChange(event: Event) {
  const value = (event.target as HTMLSelectElement).value;
  if (value === "") {
    table.setGrouping([]);
    table.setSorting([]);
  } else {
    table.setGrouping([value]);
    table.setSorting([{ id: value, desc: false }]);
  }
  table.setExpanded(true);
}

function tokenizeSearchQuery(query: string): string[] {
  return Array.from(query.matchAll(/"([^"]*)"|(\S+)/g), (match) => match[1] ?? match[2] ?? "")
    .filter((token) => token.length > 0);
}

const multiWordRowFilter: FilterFn<typeof features, PullRequest> = (
  row,
  _columnId,
  filterValue,
) => {
  const query = String(filterValue ?? "").trim().toLowerCase();
  if (query === "") {
    return true;
  }

  const tokens = tokenizeSearchQuery(query);
  if (tokens.length === 0) {
    return true;
  }

  const pr = row.original;
  const fields = [
    hasNoteStar(pr) ? `${pr.number}*` : String(pr.number),
    pr.status,
    pr.mergeable,
    pr.title,
    pr.author,
    formatAssignees(pr),
    pr.approver ?? "",
    String(pr.priority),
    pr.rollup ?? "",
  ].map((value) => value.toLowerCase());

  return tokens.every((token) => fields.some((field) => field.includes(token)));
};

const table = useTable({
  features,
  columns,
  data: computed(() => props.prs),
  getRowId: (pr: PullRequest) => String(pr.number),
  enableRowSelection: (row) => !row.getIsGrouped() && row.original.is_rollupable,
  globalFilterFn: multiWordRowFilter,
  groupedColumnMode: false,
  autoResetExpanded: false,
  initialState: {
    globalFilter: "",
    expanded: true,
    rowSelection: initialRowSelection,
    pagination: {
      pageIndex: 0,
      pageSize: DEFAULT_PAGE_SIZE,
    },
  },
});

const selectedPrNumbers = computed(() => table.getSelectedRowIds().map(Number));
watch(
  selectedPrNumbers,
  (nums) => {
    emit("selectedChange", nums);
  },
  { immediate: true },
);

const filteredSelectedCount = computed(
  () => table.getFilteredSelectedRowModel().flatRows.length,
);

const pageCount = computed(() => table.getPageCount());
const currentPage = computed(() => table.atoms.pagination.get().pageIndex);

watch(pageCount, (totalPages) => {
  const maxIndex = Math.max(0, totalPages - 1);
  if (currentPage.value > maxIndex) {
    table.setPageIndex(maxIndex);
  }
});

// Build up to 7 pagination items, using ellipses to collapse skipped pages.
// Uses 0-based page indexes to match TanStack Table.
const pageItems = computed<PageItem[]>(() => {
  const total = pageCount.value;
  const current = currentPage.value;
  if (total <= 0) {
    return [];
  }
  if (total <= 7) {
    return Array.from({ length: total }, (_, i) => i);
  }
  if (current < 4) {
    return [0, 1, 2, 3, 4, "ellipsis", total - 1];
  }
  if (current > total - 5) {
    return [0, "ellipsis", total - 5, total - 4, total - 3, total - 2, total - 1];
  }
  return [0, "ellipsis", current - 1, current, current + 1, "ellipsis", total - 1];
});

const paginationInfoText = computed(() => {
  const totalEntries = props.prs.length;
  const filteredEntries = table.getFilteredRowModel().rows.length;
  const prePaginatedRows = table.getPrePaginatedRowModel().rows;
  const pageRows = table.getRowModel().rows;
  const { pageIndex, pageSize } = table.atoms.pagination.get();

  const prRowsBefore = prePaginatedRows
    .slice(0, pageIndex * pageSize)
    .reduce(
      (count, row) =>
        count + (row.getIsGrouped() ? (row.getIsExpanded() ? row.subRows.length : 0) : 1),
      0,
    );
  const prRowsOnPage = pageRows.filter((row) => !row.getIsGrouped()).length;

  const start = prRowsOnPage === 0 ? 0 : prRowsBefore + 1;
  const end = prRowsOnPage === 0 ? 0 : prRowsBefore + prRowsOnPage;
  const base = `Showing ${start} to ${end} of ${filteredEntries} entries`;

  if (filteredEntries < totalEntries) {
    return `${base} (filtered from ${totalEntries} total entries)`;
  }
  return base;
});

function formatGroupLabel(value: unknown): string {
  const text = value === null || value === undefined ? "" : String(value);
  return text === "" ? "(none)" : text;
}

const hoveredRollupMembers = ref<Set<number>>(new Set());

function handleRowMouseEnter(pr: PullRequest) {
  hoveredRollupMembers.value = isRollup(pr) ? new Set(pr.rollup_members) : new Set();
}

function handleTableMouseLeave() {
  hoveredRollupMembers.value = new Set();
}

function handleRowClick(event: MouseEvent, row: Row<typeof features, PullRequest>) {
  const target = event.target as HTMLElement | null;
  if (!target || target.closest("a, td.select-checkbox")) {
    return;
  }

  const pr = row.original;
  if (isRollup(pr)) {
    table.setRowSelection((prev) => {
      const next = { ...prev };
      for (const memberNum of pr.rollup_members) {
        if (rollupableNumbers.value.has(memberNum)) {
          next[String(memberNum)] = true;
        }
      }
      return next;
    });
    return;
  }

  if (row.getCanSelect()) {
    row.toggleSelected();
  }
}

function isRowTreeClosed(pr: PullRequest): boolean {
  return (
    props.treeClosedPriority !== null &&
    pr.priority < props.treeClosedPriority &&
    pr.status !== "pending"
  );
}

function isRowRollupHovered(row: Row<typeof features, PullRequest>): boolean {
  if (row.getIsSelected()) {
    return false;
  }
  const num = row.original.number;
  return hoveredRollupMembers.value.has(num) || props.hoveredBuildPr === num;
}

function computeRowTitle(pr: PullRequest): string | undefined {
  const parts: string[] = [];
  if (isRollup(pr)) {
    parts.push("Select PRs of this rollup");
  }
  if (pr.note) {
    parts.push(`Note: ${pr.note}`);
  }
  return parts.length > 0 ? parts.join("\n") : undefined;
}

function pendingProgressPct(pr: PullRequest): number {
  const pending = pendingByPr.value.get(pr.number);
  if (!pending) {
    return 0;
  }
  return calculateProgressPct(
    props.nowMs - pending.created_at_ms,
    props.averageBuildDurationMs,
  );
}

function pendingElapsedText(pr: PullRequest): string {
  const pending = pendingByPr.value.get(pr.number);
  if (!pending) {
    return "";
  }
  return ` (${formatDurationMs(props.nowMs - pending.created_at_ms)})`;
}

function pendingWorkflowUrl(pr: PullRequest): string | undefined {
  return pendingByPr.value.get(pr.number)?.workflow?.url;
}

function pendingStatusTitle(pr: PullRequest): string | undefined {
  const pending = pendingByPr.value.get(pr.number);
  if (!pending) {
    return undefined;
  }
  const started = formatTimestamp(pending.created_at_ms);
  const expectedEnd = formatTimestamp(
    pending.created_at_ms + props.averageBuildDurationMs,
  );
  return `Started at: ${started}\nExpected end: ${expectedEnd}`;
}

const MARKER_CLASS: Record<string, string> = {
  yes: "positive",
  no: "negative",
  rollup: "special1",
  always: "positive",
  iffy: "neutral",
  never: "negative",
};

function sortIndicator(direction: false | "asc" | "desc"): string {
  if (direction === "asc") {
    return " ▲";
  }
  if (direction === "desc") {
    return " ▼";
  }
  return "";
}
</script>

<template>
  <div class="table-controls">
    <label>
      Search:
      <input
        :value="String(table.atoms.globalFilter.get() ?? '')"
        type="search"
        @input="table.setGlobalFilter(($event.target as HTMLInputElement).value)"
      />
    </label>

    <label>
      Group by:
      <select :value="table.atoms.grouping.get()[0] ?? ''" @change="handleGroupByChange">
        <option value="">None</option>
        <option value="status">Status</option>
        <option value="mergeable">Mergeable</option>
        <option value="author">Author</option>
        <option value="priority">Priority</option>
      </select>
    </label>
  </div>

  <div class="table-wrapper">
    <table class="data-table queue-table">
      <thead>
        <tr v-for="headerGroup in table.getHeaderGroups()" :key="headerGroup.id">
          <th
            v-for="header in headerGroup.headers"
            :key="header.id"
            :class="{
              'select-checkbox': header.column.id === 'select',
              'num-col': header.column.id === 'number' || header.column.id === 'priority',
              'status-col': header.column.id === 'status',
              title: header.column.id === 'title',
              sortable: header.column.getCanSort(),
            }"
            @click="header.column.getToggleSortingHandler()?.($event)"
          >
            <input
              v-if="header.column.id === 'select'"
              type="checkbox"
              aria-label="Select all rollupable pull requests"
              :checked="filteredSelectedCount > 0 && table.getIsAllRowsSelected()"
              :indeterminate="filteredSelectedCount > 0 && !table.getIsAllRowsSelected()"
              @change="table.getToggleAllRowsSelectedHandler()($event)"
            />
            <template v-else-if="!header.isPlaceholder">
              {{ header.column.columnDef.header }}
              <span class="sort-indicator">{{
                sortIndicator(header.column.getIsSorted())
              }}</span>
            </template>
          </th>
        </tr>
      </thead>

      <tbody @mouseleave="handleTableMouseLeave">
        <tr v-if="table.getRowModel().rows.length === 0">
          <td :colspan="table.getAllLeafColumns().length" class="empty-row">
            {{ props.prs.length === 0 ? "No data available in table" : "No matching records found" }}
          </td>
        </tr>

        <template v-for="row in table.getRowModel().rows" :key="row.id">
          <tr
            v-if="row.getIsGrouped()"
            class="group-row"
            @mouseenter="handleTableMouseLeave"
            @click="row.toggleExpanded()"
          >
            <td :colspan="table.getAllLeafColumns().length">
              <span class="group-toggle">{{ row.getIsExpanded() ? "▼" : "▶" }}</span>
              {{ formatGroupLabel(row.groupingValue) }} ({{ row.subRows.length }})
            </td>
          </tr>

          <tr
            v-else
            :title="computeRowTitle(row.original)"
            :class="{
              selected: row.getIsSelected(),
              treeclosed: isRowTreeClosed(row.original),
              'rollup-hover': isRowRollupHovered(row),
              'rollup-row': isRollup(row.original),
            }"
            @mouseenter="handleRowMouseEnter(row.original)"
            @click="handleRowClick($event, row)"
          >
            <td class="select-checkbox">
              <input
                type="checkbox"
                :aria-label="`Select PR #${row.original.number}`"
                :checked="row.getIsSelected()"
                :disabled="!row.getCanSelect()"
                @change="row.getToggleSelectedHandler()($event)"
              />
            </td>

            <td class="num-col">
              <a :href="`${repoUrl}/pull/${row.original.number}`">
                {{ row.original.number }}<template v-if="hasNoteStar(row.original)">*</template>
              </a>
            </td>

            <td
              class="status"
              :class="{ 'has-progress': pendingByPr.has(row.original.number) }"
              :data-status="row.original.status"
              :title="pendingStatusTitle(row.original)"
            >
              <div
                v-if="pendingByPr.has(row.original.number)"
                class="progress-bar-bg"
                role="progressbar"
                :aria-valuenow="Math.round(pendingProgressPct(row.original))"
                aria-valuemin="0"
                aria-valuemax="100"
                aria-label="Build progress"
                :style="{ width: `calc(${pendingProgressPct(row.original)}% - 6px)` }"
              ></div>

              <div v-if="row.original.status === 'pending'">
                <a
                  v-if="pendingWorkflowUrl(row.original)"
                  :href="pendingWorkflowUrl(row.original)"
                >
                  pending
                </a>
                <template v-else>pending</template>{{ pendingElapsedText(row.original) }}
              </div>
              <template v-else>{{ row.original.status }}</template>
            </td>

            <td class="marker" :class="MARKER_CLASS[row.original.mergeable]">
              {{ row.original.mergeable }}
            </td>

            <td class="wrapped title">{{ row.original.title }}</td>
            <td>{{ row.original.author }}</td>
            <td class="wrapped limited-width">{{ formatAssignees(row.original) }}</td>
            <td>{{ row.original.approver ?? "" }}</td>
            <td class="num-col">{{ row.original.priority }}</td>

            <td class="marker" :class="MARKER_CLASS[row.original.rollup ?? '']">
              <div class="rollup-mode">
                {{ row.original.rollup ?? "" }}
                <div
                  v-if="hasSignificantPerf(row.original)"
                  class="rustc-perf"
                  title="A rustc-perf benchmark with significant results was performed on this pull request."
                >
                  <!-- Bootstrap stopwatch icon, MIT licensed -->
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    width="16"
                    height="16"
                    fill="currentColor"
                    viewBox="0 0 16 16"
                  >
                    <path
                      d="M8.5 5.6a.5.5 0 1 0-1 0v2.9h-3a.5.5 0 0 0 0 1H8a.5.5 0 0 0 .5-.5z"
                    />
                    <path
                      d="M6.5 1A.5.5 0 0 1 7 .5h2a.5.5 0 0 1 0 1v.57c1.36.196 2.594.78 3.584 1.64l.012-.013.354-.354-.354-.353a.5.5 0 0 1 .707-.708l1.414 1.415a.5.5 0 1 1-.707.707l-.353-.354-.354.354-.013.012A7 7 0 1 1 7 2.071V1.5a.5.5 0 0 1-.5-.5M8 3a6 6 0 1 0 .001 12A6 6 0 0 0 8 3"
                    />
                  </svg>
                </div>
              </div>
            </td>
          </tr>
        </template>
      </tbody>
    </table>
  </div>

  <div class="table-footer">
    <div class="table-info" role="status" aria-live="polite">
      {{ paginationInfoText }}
      <span v-if="selectedPrNumbers.length > 0" class="select-info">
        {{ selectedPrNumbers.length }} {{ selectedPrNumbers.length === 1 ? "row" : "rows" }}
        selected
      </span>
    </div>

    <nav v-if="pageCount > 0" class="pagination" aria-label="Pagination">
      <button
        type="button"
        class="paging-button"
        aria-label="First page"
        :disabled="!table.getCanPreviousPage()"
        @click="table.firstPage()"
      >
        «
      </button>
      <button
        type="button"
        class="paging-button"
        aria-label="Previous page"
        :disabled="!table.getCanPreviousPage()"
        @click="table.previousPage()"
      >
        ‹
      </button>

      <template v-for="(item, idx) in pageItems" :key="`${item}-${idx}`">
        <span v-if="item === 'ellipsis'" class="paging-ellipsis">…</span>
        <button
          v-else
          type="button"
          class="paging-button"
          :class="{ current: item === currentPage }"
          :aria-current="item === currentPage ? 'page' : undefined"
          :aria-label="`Page ${item + 1}`"
          @click="table.setPageIndex(item)"
        >
          {{ item + 1 }}
        </button>
      </template>

      <button
        type="button"
        class="paging-button"
        aria-label="Next page"
        :disabled="!table.getCanNextPage()"
        @click="table.nextPage()"
      >
        ›
      </button>
      <button
        type="button"
        class="paging-button"
        aria-label="Last page"
        :disabled="!table.getCanLastPage()"
        @click="table.lastPage()"
      >
        »
      </button>
    </nav>
  </div>
</template>

<style scoped>
@keyframes barber-pole {
  /* This animation is used to make the status indicator next to
     pending pulls look like a barber poll. We do that with a
     diagonal linear gradient. CSS does not allow us to animate a
     gradient, so instead we make the indicator a little taller
     than shown, and then animate the whole thing upward. */
  from {
    transform: translate(0, 0);
  }
  to {
    /* The magic number 11.314 is sqrt(8^2 + 8^2), based on how
       far vertically it takes to repeat a 45 degree gradient
       that is 8 pixels long before it repeats. */
    transform: translate(0, -11.314px);
  }
}

.table-controls {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.75rem;
  margin-bottom: 0.5rem;
}

.table-controls input,
.table-controls select {
  border: 1px solid var(--color-controls-border);
  border-radius: 3px;
  padding: 4px 6px;
  background-color: var(--color-controls-input);
}

.table-controls input:active,
.table-controls select:active {
  border-color: var(--color-controls-border-active);
  border-width: 3px;
  margin: -2px;
}

.group-row {
  background-color: var(--color-row-group-header);
  font-weight: 600;
  cursor: pointer;
}

.group-toggle {
  display: inline-block;
  width: 1.1em;
  font-size: 0.8em;
}

.table-wrapper {
  overflow-x: auto;
  -webkit-overflow-scrolling: touch;
}

.table-footer {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.75rem;
  min-height: 0.5rem;
  margin-top: 0.5rem;
}

.select-info {
  margin-left: 10px;
}

.pagination {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 2px;
}

.paging-button {
  box-sizing: border-box;
  display: inline-block;
  min-width: 1.5em;
  padding: 0.45em 0.9em;
  text-align: center;
  cursor: pointer;
  color: inherit;
  border: 1px solid transparent;
  border-radius: 2px;
  background: transparent;
}

.paging-button.current,
.paging-button.current:hover {
  color: var(--color-paging-current-fg);
  border: 1px solid var(--color-paging-current-bg2);
  background: linear-gradient(
    to bottom,
    var(--color-paging-current-bg1) 0%,
    var(--color-paging-current-bg2) 100%
  );
}

.paging-button:disabled {
  cursor: default;
  color: var(--color-paging-disabled-fg);
  border: 1px solid transparent;
  background: transparent;
}

.paging-button:not(:disabled):not(.current):hover {
  color: white;
  border: 1px solid var(--color-paging-hover-bg2);
  background: linear-gradient(to bottom, var(--color-paging-hover-bg1) 0%, var(--color-paging-hover-bg2) 100%);
}

.paging-ellipsis {
  padding: 0 0.5em;
}

.queue-table thead th {
  user-select: none;
}

.queue-table thead th.sortable {
  cursor: pointer;
}

.sort-indicator {
  font-size: 0.75em;
  color: var(--color-sort-indicator);
}

.queue-table tbody tr.selected > td {
  background-color: #d6e4f0;
}

.queue-table tbody tr.rollup-hover:not(.selected) > td {
  background-color: var(--color-row-selected);
}

.queue-table tbody tr.rollup-row {
  cursor: pointer;
}

.empty-row {
  text-align: center;
  padding: 0.75rem;
}

.treeclosed {
  opacity: 0.5;
}

th.select-checkbox,
td.select-checkbox {
  width: 2.5rem;
  text-align: center;
}

th.status-col {
  padding-left: 16px;
}

td.status {
  position: relative;
  overflow: hidden;
  padding-left: 10px;
  white-space: nowrap;
}

td.status > * {
  inline-size: min-content;
}

td.status::before {
  content: " ";
  position: absolute;
  display: block;
  left: 0;
  width: 6px;
  top: 0;
  bottom: 0;
}

td.status[data-status="pending"]::before {
  /* Give the pending state a little bit of animation to make it
     clear that these items are the ones that are being tested
     right now. */
  bottom: -20px;
  background-color: var(--color-pending1);
  background-image: repeating-linear-gradient(
    135deg,
    var(--color-pending1) 0,
    var(--color-pending1) 4px,
    var(--color-pending2) 4px,
    var(--color-pending2) 8px,
    var(--color-pending1) 0
  );
  animation: barber-pole 1s linear infinite;
}

td.status[data-status="approved"]::before {
  background-color: var(--color-approved)
}

/* Progress bar background for pending builds */
.progress-bar-bg {
  position: absolute;
  left: 6px; /* Start after the status indicator bar */
  top: 0;
  bottom: 0;
  width: 0;
  max-width: calc(100% - 6px);
  background-color: var(--color-pending0);
  z-index: 0;
  pointer-events: none;
  box-sizing: border-box;
}

/* Fixed vertical line at the right edge to show where progress ends */
td.status.has-progress::after {
  content: "";
  position: absolute;
  right: 0;
  top: 0;
  bottom: 0;
  width: 2px;
  background-color: var(--color-pending3);
  z-index: 0;
  pointer-events: none;
}

td.status > *:not(.progress-bar-bg) {
  position: relative;
  z-index: 1;
}

.marker {
  white-space: nowrap;
}

.marker::before {
  content: "";
  display: inline-block;
  font-size: 18px;
  width: 0.4em;
  height: 0.4em;
  border-radius: 50% 50%;
  margin-right: 0.3em;
  border: 1px solid transparent;
}

.positive {
  color: var(--color-positive-text);
}
.positive::before {
  background: var(--color-positive-dot);
  border-color: var(--color-positivea-text);
}

.negative {
  color: var(--color-negative-text);
}
.negative::before {
  background: var(--color-negative-dot);
  border-color: var(--color-negative-text);
}

.neutral {
  color: var(--color-neutral-text);
}
.neutral::before {
  background: var(--color-neutral-dot);
  border-color: var(--color-neutral-text);
}

.special1 {
  color: var(--color-special-text);
}
.special1::before {
  background: var(--color-special-dot);
  border-color: var(--color-special-text);
}

@media screen and (min-width: 800px) {
  .wrapped {
    text-wrap: wrap;
    word-break: break-word;
  }
}

.title {
  min-width: 300px;
  width: 40%;
}

.limited-width {
  max-width: 250px;
}

.rollup-mode {
  display: inline-flex;
  flex-direction: row;
  align-items: center;
}

.rustc-perf {
  display: flex;
  margin-left: 5px;
}
</style>
