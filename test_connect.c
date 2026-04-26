
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <errno.h>

int main() {
    int sockfd = socket(AF_INET, SOCK_STREAM, 0);
    if (sockfd < 0) {
        perror("socket");
        return 1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(80);
    inet_pton(AF_INET, "192.168.64.2", &addr.sin_addr);

    printf("Connecting to 192.168.64.2:80...\n");
    int ret = connect(sockfd, (struct sockaddr *)&addr, sizeof(addr));
    if (ret < 0) {
        perror("connect");
        printf("errno: %d\n", errno);
        close(sockfd);
        return 1;
    }

    printf("SUCCESS: Connected!\n");
    close(sockfd);
    return 0;
}
