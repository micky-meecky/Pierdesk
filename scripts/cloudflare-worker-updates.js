const REPO = "micky-meecky/Pierdesk";

function resolveUpstream(pathname) {
  if (pathname === "/updates/stable.json") {
    return {
      url: `https://github.com/${REPO}/releases/latest/download/pierdesk-update.json`,
      cacheControl: "public, max-age=60, must-revalidate",
      contentType: "application/json; charset=utf-8",
    };
  }
  if (pathname === "/releases/latest/pierdesk.exe") {
    return {
      url: `https://github.com/${REPO}/releases/latest/download/pierdesk.exe`,
      cacheControl: "public, max-age=300",
      contentType: "application/vnd.microsoft.portable-executable",
    };
  }
  const exeMatch = pathname.match(/^\/releases\/v(\d+\.\d+\.\d+)\/pierdesk\.exe$/);
  if (exeMatch) {
    return {
      url: `https://github.com/${REPO}/releases/download/v${exeMatch[1]}/pierdesk.exe`,
      cacheControl: "public, max-age=31536000, immutable",
      contentType: "application/vnd.microsoft.portable-executable",
    };
  }
  const manifestMatch = pathname.match(/^\/releases\/v(\d+\.\d+\.\d+)\/pierdesk-update\.json$/);
  if (manifestMatch) {
    return {
      url: `https://github.com/${REPO}/releases/download/v${manifestMatch[1]}/pierdesk-update.json`,
      cacheControl: "public, max-age=31536000, immutable",
      contentType: "application/json; charset=utf-8",
    };
  }
  return null;
}

export default {
  async fetch(request) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method Not Allowed", { status: 405 });
    }
    const { pathname } = new URL(request.url);
    if (pathname === "/health") {
      return new Response(
        JSON.stringify({ ok: true, service: "pierdesk-updates", repo: REPO }),
        {
          status: 200,
          headers: {
            "content-type": "application/json; charset=utf-8",
            "cache-control": "no-store",
            "x-content-type-options": "nosniff",
          },
        }
      );
    }
    const target = resolveUpstream(pathname);
    if (!target) {
      return new Response("Not Found", { status: 404 });
    }
    const upstream = await fetch(target.url, {
      method: request.method,
      redirect: "follow",
      headers: { "User-Agent": "Pierdesk-Update-Mirror" },
    });
    const headers = new Headers();
    for (const key of ["content-length", "etag", "last-modified", "content-disposition", "accept-ranges"]) {
      const value = upstream.headers.get(key);
      if (value) headers.set(key, value);
    }
    headers.set("content-type", upstream.ok ? target.contentType : (upstream.headers.get("content-type") || "text/plain; charset=utf-8"));
    headers.set("cache-control", upstream.ok ? target.cacheControl : "no-store");
    headers.set("x-content-type-options", "nosniff");
    return new Response(request.method === "HEAD" ? null : upstream.body, {
      status: upstream.status,
      headers,
    });
  },
};
