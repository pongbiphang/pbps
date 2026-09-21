/* Keep a worker alive after its leader exits. This is a measurement helper,
 * not part of the resolver or a process installed on a target. */
#define _GNU_SOURCE
#include <pthread.h>
#include <unistd.h>

static void *worker(void *unused) {
    (void)unused;
    sleep(300);
    return NULL;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL)) return 1;
    sleep(1);
    pthread_exit(NULL);
}
