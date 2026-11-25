// Skip interval ticks while a poll is pending, so an old response cannot
// replace newer progress or move the log cursor backwards.
export function singleFlight(task) {
  let pending = false;
  return async (...args) => {
    if (pending) return;
    pending = true;
    try {
      return await task(...args);
    } finally {
      pending = false;
    }
  };
}
