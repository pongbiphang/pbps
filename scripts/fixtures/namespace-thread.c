/* Keep a worker alive after its leader exits. This is a measurement helper,
 * not part of the resolver or a process installed on a target. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void *worker(void *unused) {
    (void)unused;
    sleep(300);
    return NULL;
}

static pthread_barrier_t credentials_ready;

static void *root_worker(void *unused) {
    (void)unused;
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct caps[2] = {{0}};
    if (syscall(SYS_capset, &header, caps)) _exit(1);
    pthread_barrier_wait(&credentials_ready);
    sleep(300);
    return NULL;
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "mapping-ranges")) {
        int fd = open(argv[2], O_RDONLY);
        long length = sysconf(_SC_PAGESIZE);
        if (fd < 0 || length <= 0) return 1;
        void *first = mmap(NULL, length, PROT_READ, MAP_PRIVATE, fd, 0);
        void *second = mmap(NULL, length, PROT_READ, MAP_PRIVATE, fd, 0);
        if (first == MAP_FAILED || second == MAP_FAILED) return 1;
        close(fd);
        void *low = (uintptr_t)first < (uintptr_t)second ? first : second;
        void *high = low == first ? second : first;
        puts("ready");
        fflush(stdout);
        if (getchar() != '1' || munmap(low, length)) return 1;
        puts("one");
        fflush(stdout);
        if (getchar() != '2' || munmap(high, length)) return 1;
        puts("none");
        fflush(stdout);
        return getchar() == 'q' ? 0 : 1;
    }
    if (argc == 2 && !strcmp(argv[1], "opaque-name")) {
        int ready[2];
        if (pipe(ready)) return 1;
        pid_t child = fork();
        if (child < 0) return 1;
        if (!child) {
            close(ready[0]);
            if (prctl(PR_SET_NAME, "odd\xff)(\n\t\\name", 0, 0, 0)) _exit(1);
            if (write(ready[1], "1", 1) != 1) _exit(1);
            close(ready[1]);
            sleep(300);
            _exit(0);
        }
        close(ready[1]);
        char byte;
        if (read(ready[0], &byte, 1) != 1 || byte != '1') return 1;
        close(ready[0]);
        FILE *signal = fopen("/dev/shm/name-ready", "w");
        if (!signal) return 1;
        fprintf(signal, "%d\n", child);
        if (fclose(signal)) return 1;
        sleep(300);
        return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "many")) {
        pthread_t threads[96];
        for (unsigned int i = 0; i < sizeof(threads) / sizeof(threads[0]); ++i) {
            if (pthread_create(&threads[i], NULL, worker, NULL)) return 1;
        }
        if (prctl(PR_SET_NAME, "pbps-many", 0, 0, 0)) return 1;
        sleep(300);
        return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "orphan")) {
        FILE *release = fopen("/dev/shm/release", "w");
        if (!release || fclose(release)) return 1;
        pid_t parent = fork();
        if (parent < 0) return 1;
        if (!parent) {
            pid_t child = fork();
            if (child < 0) _exit(1);
            if (!child) {
                FILE *ready = fopen("/dev/shm/ready", "w");
                if (!ready) _exit(1);
                fprintf(ready, "%d %d\n", getppid(), getpid());
                if (fclose(ready)) _exit(1);
                sleep(300);
                _exit(0);
            }
            for (;;) {
                FILE *signal = fopen("/dev/shm/release", "r");
                if (!signal) _exit(1);
                int byte = fgetc(signal);
                fclose(signal);
                if (byte == '1') _exit(0);
                usleep(1000);
            }
        }
        if (waitpid(parent, NULL, 0) != parent) return 1;
        sleep(300);
        return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "mixed")) {
        // Linux credentials belong to tasks. Bypass libc's all-thread UID
        // synchronization to retain a root worker beneath a UID-999 leader.
        for (int cap = 0; cap < 64; ++cap) {
            if (prctl(PR_CAPBSET_DROP, cap, 0, 0, 0) && errno != EINVAL) return 1;
        }
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 1;
        if (pthread_barrier_init(&credentials_ready, NULL, 2)) return 1;
        pthread_t thread;
        if (pthread_create(&thread, NULL, root_worker, NULL)) return 1;
        if (syscall(SYS_setresuid, 999, 999, 999)) return 1;
        pthread_barrier_wait(&credentials_ready);
        if (prctl(PR_SET_NAME, "pbps-mixed", 0, 0, 0)) return 1;
        sleep(300);
        return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "churn")) {
        if (prctl(PR_SET_NAME, "pbps-churn", 0, 0, 0)) return 1;
        for (;;) {
            pid_t child = fork();
            if (child < 0) return 1;
            if (!child) {
                usleep(100);
                _exit(0);
            }
            if (waitpid(child, NULL, 0) != child) return 1;
        }
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL)) return 1;
    sleep(1);
    pthread_exit(NULL);
}
