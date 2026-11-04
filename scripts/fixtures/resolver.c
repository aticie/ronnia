/* Process-local DNS failure injection for the executable smoke test only.
 * No resolver configuration or /etc/hosts writes. LD_PRELOAD this shared object
 * into the tested service, never into the harness or a real deployment.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <netdb.h>
#include <stdatomic.h>
#include <string.h>
#include <unistd.h>

static _Atomic unsigned int attempts;

typedef int (*resolver_fn)(const char *, const char *,
                           const struct addrinfo *, struct addrinfo **);

int getaddrinfo(const char *node, const char *service,
                const struct addrinfo *hints, struct addrinfo **result)
{
    resolver_fn resolve = (resolver_fn)dlsym(RTLD_NEXT, "getaddrinfo");
    if (!resolve)
        return EAI_SYSTEM;
    if (node && strcmp(node, "recover.localhost") == 0) {
        static const char attempt_marker[] = "RONNIA_SMOKE_DNS_ATTEMPT\n";
        (void)write(STDERR_FILENO, attempt_marker, sizeof(attempt_marker) - 1);
        unsigned int attempt = atomic_fetch_add_explicit(&attempts, 1, memory_order_relaxed);
        if (attempt < 2) {
            static const char marker[] = "RONNIA_SMOKE_DNS_FAILURE\n";
            (void)write(STDERR_FILENO, marker, sizeof(marker) - 1);
            if (result)
                *result = 0;
            return EAI_NONAME;
        }
        /* Resolve the configured host locally so the same process exercises the
         * real connection-refused path before the fixture server starts listening. */
        return resolve("127.0.0.1", service, hints, result);
    }
    return resolve(node, service, hints, result);
}
