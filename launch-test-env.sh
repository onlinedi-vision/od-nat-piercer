#!/usr/bin/env bash

set -euo pipefail

scan_flag=''

function print_usage() {
	echo "Usage: ./launch-test-env.sh [OPTION]"
	echo "OPTION:"
	echo "   -s    scan the resulting image with Trivy"
	echo "   -h    display this message"
}

while getopts 'sh' flag; do
	case "${flag}" in
	s)
		scan_flag='true'
		;;
	h)
		print_usage
		exit 0
		;;
	*)
		print_usage
		exit 1
		;;
	esac
done

export GIT_BRANCH="${GIT_BRANCH:-refs/heads/test-env}"

IMAGE_TAG="${IMAGE_TAG:-od-nat-piercer:${GIT_COMMIT:-test-env}}"

echo "================ BUILDING AND TESTING ================="

docker buildx bake \
	-f docker-bake.hcl \
	--set release.output='type=docker' \
	--set "release.tags=${IMAGE_TAG}"

if [[ "${scan_flag}" == "true" ]]; then
	echo "================ SCANNING IMAGE ================="

	docker run --rm \
		-v /var/run/docker.sock:/var/run/docker.sock \
		aquasec/trivy:0.36.0 image \
		--format table \
		--exit-code 1 \
		--ignore-unfixed \
		--vuln-type os,library \
		--severity CRITICAL,HIGH \
		"${IMAGE_TAG}"
fi