variable "GIT_BRANCH" {}

group "default" {
    targets = ["release"]
}


function "tag" {
    params = [branch]
    result = "v${split("/", branch)[2]}"
}

target "release" {
    target = "runtime"

    contexts = {
        bare-repo = "rootfs/repo"
    }

    output = ["type=cacheonly"]

    tags = ["registry.onlinedi.vision:5000/od-nat-piercer:${tag(GIT_BRANCH)}"]
}