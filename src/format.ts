/**
 * 时间格式化的小工具。
 *
 * 额度到期时间在界面上有两处用途（账号列表的副标题、接管页选号提示），
 * 都是「日粒度」——积分包的到期只按自然日比大小，所以统一收在这里，避免各页各写一份。
 */

/** 毫秒 → `MM-DD` */
export function mmdd(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/**
 * 毫秒 → `MM-DD HH:MM`。
 *
 * token 有效期是「小时」量级（自动续签在到期前 24 小时内动手），只给 `MM-DD` 会看不出
 * 「还剩几小时」，所以这里保留到分钟。
 */
export function stamp(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * 距今天还有几个自然日：今天到期 = 0、已过期 < 0。
 *
 * 先各自归零到当天 0 点再相减 —— 直接减毫秒会因「现在几点」把 23 小时算成 0 天。
 */
export function daysUntil(ms: number): number {
  const target = new Date(ms);
  target.setHours(0, 0, 0, 0);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  return Math.round((target.getTime() - today.getTime()) / 86_400_000);
}
