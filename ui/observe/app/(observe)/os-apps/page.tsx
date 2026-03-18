import { redirect } from "next/navigation";

/** Backward-compatible redirect: /os-apps -> /skills */
export default function OsAppsPage() {
  redirect("/skills");
}
