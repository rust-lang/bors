import {createApp} from "vue";
import Queue from "./Queue.vue";

document.addEventListener("DOMContentLoaded", (_event) => {
    const data: Data = JSON.parse(document.getElementById("queue-data")!.textContent);
    createApp(Queue, {data}).mount("#queue-app");
});
