pipeline {
  agent any
  

  stages {
	  
	  stage('Docker Kill') {
		  steps {
				sh 'docker compose down' 		  
			}
	  }

	  stage('Docker Build') {
		  steps {
		  	sh 'docker compose build'
     	 }
	  }
   	stage('Docker Run') {
		 steps {
        		sh 'docker compose up -d'
					}
		}
	}
}


