def imageTag(){
	def branchName = env.GIT_BRANCH.tokenize('/').last()
	return "registry.onlinedi.vision:5000/od-nat-piercer:v${branchName}"
}

pipeline {
	agent any

	options {
		disableConcurrentBuilds()
		timeout(time: 2, unit: 'HOURS')
	}

	stages {
		stage('Push Image') {
			when {
				expression { env.GIT_BRANCH ==~ /^refs\/tags\/\d+\.\d+\.\d+$/ }
			}

			steps {
				script {
					withDockerRegistry(
						url: 'https://registry.onlinedi.vision:5000',
						credentialsId: 'docker-registry'
					) {
						sh 'docker buildx bake -f docker-bake.hcl --set release.output=type=registry'
					}
				}
			}
		}

		stage('Deploy'){
			when {
				expression { env.GIT_BRANCH ==~ /^refs\/tags\/\d+\.\d+\.\d+$/ }
			}

			steps {
				script {
					def tag = imageTag()

					withDockerRegistry(
						url: 'https://registry.onlinedi.vision:5000',
						credentialsId: 'docker-registry'
					){
						sh "OD_NAT_PIERCER_IMAGE='${tag}' docker compose up -d --remove-orphans"
					}
				}
			}
		}
	}

	post {
		failure {
			emailext(
				from: 'jenkins@mail.onlinedi.vision',
				subject: "Build Failed: ${env.JOB_NAME} - ${env.BUILD_NUMBER}",
				body: "Check ${env.BUILD_URL}",
				to: 'TEAM@mail.onlinedi.vision'
			)
		}
	}
}
