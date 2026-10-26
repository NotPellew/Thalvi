/** Keeps the screen on; returns a release function. Silent where unsupported. */
export function holdWakeLock(): () => void {
  if (!("wakeLock" in navigator)) return () => undefined;
  let sentinel: WakeLockSentinel | null = null;
  let held = true;

  const acquire = async () => {
    const next = await navigator.wakeLock.request("screen").catch(() => null);
    if (!held) await next?.release();
    else sentinel = next;
  };
  // The browser drops the lock whenever the page is hidden.
  const onVisible = () => {
    if (document.visibilityState === "visible" && held) void acquire();
  };
  document.addEventListener("visibilitychange", onVisible);
  void acquire();

  return () => {
    held = false;
    document.removeEventListener("visibilitychange", onVisible);
    void sentinel?.release();
  };
}
