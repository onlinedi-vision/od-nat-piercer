def imageTag(){
	return "registry.onlinedi.vision:5000/od-nat-piercer:${env.GIT_COMMIT}"
}

def buildAndScanImage ={
	def tag = imageTag()

	sh """
		GIT_BRANCH='refs/tags/0.0.0' docker buildx bake \
		--set release.output='type=docker' \
		--set release.tags='${tag}'
	"""
	sh """
		docker run --rm \
		-v /var/run/docker.sock:/var/run/docker.sock \
		aquasec/trivy:0.36.0 image \
		--format table \
		--exit-code 1 \
		--ignore-unfixed \
		--vuln-type os,library \
		--severity CRITICAL,HIGH \
		'${tag}'
	"""
}

pipeline {
	agent any
	
	options {
		disableConcurrentBuilds()
		timeout(time: 2, unit: 'HOURS')
	}

	environment {
		CARGO_TERM_COLOR = 'always'
	}

	stages {
		stage('Test'){
			steps {
				sh 'cargo test --locked'
			}
		}

		stage('Build and Scan'){
			steps {
				script{
					buildAndScanImage()
				}
			}
		}

		stage('Push Image') {
			when{
				allOf {
					branch 'main'
					not { changeRequest() }
				}
			}

			steps {
				script{
					withDockerRegistry(
						url: 'https://registry.onlinedi.vision:5000',
						credentialsId: 'docker-registry'
					) {
						sh "docker push '${imageTag()}'"
					}
				}
			}
		}
	}

	post {
		always {
			script {
				sh "docker image rm '${imageTag()}' || true"
			}
		}

		failure{
			emailext(
				from: 'jenkins@mail.onlinedi.vision',
				subject: "Build Failed: ${env.JOB_NAME} - ${env.BUILD_NUMBER}",
				body: "Check ${env.BUILD_URL}",
				to: 'TEAM@mail.onlinedi.vision'
			)
		}
	}
}